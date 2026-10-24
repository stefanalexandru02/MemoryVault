//! Result appliers (spec 12.2, 12.3): every decision type, the rejection rules, audit rows.

use memvault_core::ids::{new_id, short_id};
use memvault_core::schemas::common::PacketKind;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::*;

/// action, target, actor, before, after, reason, prompt version.
type AuditRow = (String, Uuid, String, Option<Value>, Option<Value>, Option<String>, Option<String>);

struct World {
    c: Ctx,
    space: Uuid,
    subject: Uuid,
    run: Uuid,
}

async fn world(pool: PgPool) -> World {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    World { c, space: alice.space_id, subject, run: new_id() }
}

impl World {
    async fn active(&self, spec: FactSpec) -> Uuid {
        fact(&self.c.pool, self.space, self.subject, FactSpec { status: "active", ..spec }).await
    }

    async fn candidate(&self, spec: FactSpec) -> Uuid {
        fact(&self.c.pool, self.space, self.subject, spec).await
    }

    async fn facts_packet(&self, ids: &[Uuid]) -> memvault_core::schemas::work::PacketRecord {
        self.c.packet(self.space, PacketKind::ConsolidateFacts, run_inputs(self.run, ids)).await
    }

    async fn obs(&self, title: &'static str) -> Uuid {
        observation(&self.c.pool, self.space, ObsSpec { title, ..Default::default() }).await
    }
}

fn decisions(d: Value) -> Value {
    json!({ "decisions": d })
}

async fn row(pool: &PgPool, id: Uuid) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(f) - 'search_text' - 'search_tsv' FROM facts f WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn supersede_replaces_the_active_fact(pool: PgPool) {
    let w = world(pool.clone()).await;
    let old = w.active(FactSpec { value_text: "vim", confidence: 0.8, ..Default::default() }).await;
    let new = w
        .candidate(FactSpec { value_text: "emacs", conflict: true, supersedes_id: Some(old), ..Default::default() })
        .await;
    let packet = w.facts_packet(&[old, new]).await;

    let counts =
        w.c.apply(
            &packet,
            decisions(json!([{"action": "supersede", "target_id": old, "by_candidate_id": new,
                              "valid_from": "2026-10-01", "reason": "user switched on two devices"}])),
        )
        .await;
    assert_eq!(counts["supersede"], 1);
    assert_eq!(counts["decisions"], 1);
    assert!(!counts.contains_key("rejected"));

    let (o, n) = (row(&pool, old).await, row(&pool, new).await);
    assert_eq!(o["status"], "superseded");
    assert_eq!(o["superseded_by"], json!(new));
    assert!(o["valid_to"].as_str().unwrap().starts_with("2026-10-01T00:00:00"));
    assert_eq!(n["status"], "active");
    assert_eq!(n["supersedes_id"], json!(old));
    assert_eq!(n["conflict"], false);
    assert!(n["valid_from"].as_str().unwrap().starts_with("2026-10-01T00:00:00"));

    // audit: before/after snapshots, the packet's run id and actor
    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT action, target_id, actor, before, after, reason, prompt_version FROM memory_audit
              WHERE run_id = $1 ORDER BY seq",
    )
    .bind(w.run)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].0.as_str(), rows[0].1), ("fact.supersede", old));
    assert_eq!((rows[1].0.as_str(), rows[1].1), ("fact.create", new));
    assert_eq!(rows[0].2, format!("packet:{}", packet.id));
    assert_eq!(rows[0].3.as_ref().unwrap()["status"], "active");
    assert_eq!(rows[0].4.as_ref().unwrap()["status"], "superseded");
    assert_eq!(rows[1].3.as_ref().unwrap()["status"], "candidate");
    assert_eq!(rows[1].4.as_ref().unwrap()["status"], "active");
    assert_eq!(rows[0].5.as_deref(), Some("user switched on two devices"));
    assert_eq!(rows[0].6.as_deref(), Some("consolidate.test@v1"));

    // the profile of the scope now shows the new value only
    let profile: String = sqlx::query_scalar("SELECT body_md FROM profiles WHERE space_id = $1 AND scope_key = 'acme'")
        .bind(w.space)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(profile.contains("emacs") && !profile.contains("vim"), "{profile}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn supersede_rules(pool: PgPool) {
    let w = world(pool.clone()).await;
    let pinned =
        w.active(FactSpec { pinned: true, predicate: "address.me", value_text: "Dr Alex", ..Default::default() }).await;
    let pinned_new = w
        .candidate(FactSpec {
            predicate: "address.me",
            value_text: "Alex",
            conflict: true,
            supersedes_id: Some(pinned),
            ..Default::default()
        })
        .await;
    let old = w.active(FactSpec::default()).await;
    let other_active = w.active(FactSpec { scope_key: "other", ..Default::default() }).await;
    let new_other = w.candidate(FactSpec { scope_key: "other", value_text: "emacs", ..Default::default() }).await;
    let cand_a = w.candidate(FactSpec { value_text: "helix", ..Default::default() }).await;
    let packet = w.facts_packet(&[pinned, pinned_new, old, other_active, new_other, cand_a]).await;

    let counts = w
        .c
        .apply(
            &packet,
            decisions(json!([
                // pinned target
                {"action": "supersede", "target_id": pinned, "by_candidate_id": pinned_new, "reason": "r"},
                // the replacement's key is held by another active fact
                {"action": "supersede", "target_id": old, "by_candidate_id": new_other, "reason": "r"},
                // target is not active
                {"action": "supersede", "target_id": cand_a, "by_candidate_id": new_other, "reason": "r"},
                // bad date
                {"action": "supersede", "target_id": old, "by_candidate_id": cand_a, "valid_from": "someday", "reason": "r"},
                // not a UUID of the cluster
                {"action": "supersede", "target_id": "nope", "by_candidate_id": cand_a, "reason": "r"},
            ])),
        )
        .await;
    assert_eq!(counts["rejected"], 5, "{counts:?}");
    assert!(!counts.contains_key("supersede"));
    for (id, status) in [
        (pinned, "active"),
        (pinned_new, "candidate"),
        (old, "active"),
        (new_other, "candidate"),
        (cand_a, "candidate"),
    ] {
        assert_eq!(status_of(&pool, id).await, status);
    }
    assert_eq!(count(&pool, "SELECT count(*) FROM memory_audit").await, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn merge_into_adds_evidence_and_combines_confidence(pool: PgPool) {
    let w = world(pool.clone()).await;
    let (e1, e2, e3) = (w.obs("one").await, w.obs("two").await, w.obs("three").await);
    let target =
        w.active(FactSpec { confidence: 0.6, confirmations: 2, evidence: vec![e1, e2], ..Default::default() }).await;
    let cand = w
        .candidate(FactSpec {
            value_text: "Vim editor",
            confidence: 0.6,
            confirmations: 3,
            evidence: vec![e2, e3],
            ..Default::default()
        })
        .await;
    let packet = w.facts_packet(&[target, cand]).await;

    let counts =
        w.c.apply(
            &packet,
            decisions(
                json!([{"action": "merge_into", "target_id": target, "candidate_id": cand, "reason": "same fact"}]),
            ),
        )
        .await;
    assert_eq!(counts["merge_into"], 1);
    let t = row(&pool, target).await;
    assert_eq!(t["status"], "active");
    assert_eq!(t["confirmations"], 5);
    let mut evidence: Vec<String> =
        t["evidence_observation_ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    evidence.sort();
    let mut want = vec![e1.to_string(), e2.to_string(), e3.to_string()];
    want.sort();
    assert_eq!(evidence, want);
    // 1 - (1 - 0.6)(1 - 0.6) = 0.84
    assert!((t["confidence"].as_f64().unwrap() - 0.84).abs() < 1e-6, "{t}");
    let c = row(&pool, cand).await;
    assert_eq!(c["status"], "superseded");
    assert_eq!(c["superseded_by"], json!(target));
    assert_eq!(audit_actions(&pool, w.run).await, vec!["fact.confirm", "fact.supersede"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn merge_caps_confidence_and_leaves_pinned_alone(pool: PgPool) {
    let w = world(pool.clone()).await;
    let high = w.active(FactSpec { confidence: 0.95, ..Default::default() }).await;
    let cand = w.candidate(FactSpec { confidence: 0.95, ..Default::default() }).await;
    let pinned =
        w.active(FactSpec { pinned: true, confidence: 0.7, predicate: "address.me", ..Default::default() }).await;
    let cand2 = w.candidate(FactSpec { confidence: 0.9, predicate: "address.me", ..Default::default() }).await;
    let packet = w.facts_packet(&[high, cand, pinned, cand2]).await;
    let counts =
        w.c.apply(
            &packet,
            decisions(json!([
                {"action": "merge_into", "target_id": high, "candidate_id": cand, "reason": "r"},
                {"action": "merge_into", "target_id": pinned, "candidate_id": cand2, "reason": "r"},
            ])),
        )
        .await;
    assert_eq!(counts["merge_into"], 2);
    assert!((row(&pool, high).await["confidence"].as_f64().unwrap() - 0.98).abs() < 1e-6);
    let p = row(&pool, pinned).await;
    assert!((p["confidence"].as_f64().unwrap() - 0.7).abs() < 1e-6, "pinned confidence is kept");
    assert_eq!(p["confirmations"], 2, "but the confirmation is counted");
    assert_eq!(status_of(&pool, cand2).await, "superseded");
}

#[sqlx::test(migrations = "../../migrations")]
async fn keep_both_activates_candidates_side_by_side(pool: PgPool) {
    let w = world(pool.clone()).await;
    let laptop =
        w.active(FactSpec { scope_kind: "device", scope_key: "laptop", value_text: "vim", ..Default::default() }).await;
    let desktop = w
        .candidate(FactSpec {
            scope_kind: "device",
            scope_key: "desktop",
            value_text: "emacs",
            conflict: true,
            supersedes_id: Some(laptop),
            ..Default::default()
        })
        .await;
    let packet = w.facts_packet(&[laptop, desktop]).await;
    let counts =
        w.c.apply(
            &packet,
            decisions(json!([{"action": "keep_both", "ids": [laptop, desktop], "reason": "different machines"}])),
        )
        .await;
    assert_eq!(counts["keep_both"], 1);
    let d = row(&pool, desktop).await;
    assert_eq!(d["status"], "active");
    assert_eq!(d["conflict"], false);
    assert!(d["supersedes_id"].is_null());
    assert_eq!(status_of(&pool, laptop).await, "active");
    assert_eq!(audit_actions(&pool, w.run).await, vec!["fact.create"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn keep_both_refuses_to_duplicate_a_key(pool: PgPool) {
    let w = world(pool.clone()).await;
    let active = w.active(FactSpec::default()).await;
    let same_key = w
        .candidate(FactSpec { value_text: "emacs", conflict: true, supersedes_id: Some(active), ..Default::default() })
        .await;
    let packet = w.facts_packet(&[active, same_key]).await;
    let counts =
        w.c.apply(&packet, decisions(json!([{"action": "keep_both", "ids": [active, same_key], "reason": "both"}])))
            .await;
    assert_eq!(counts["rejected"], 1);
    assert_eq!(status_of(&pool, same_key).await, "candidate");
}

#[sqlx::test(migrations = "../../migrations")]
async fn retract_requires_cited_evidence(pool: PgPool) {
    let w = world(pool.clone()).await;
    let evidence = w.obs("the user said the opposite").await;
    let other = seed(&pool).await;
    let foreign = observation(&pool, other.space_id, ObsSpec::default()).await;
    let target = w.active(FactSpec::default()).await;
    let packet = w.facts_packet(&[target]).await;

    let counts =
        w.c.apply(
            &packet,
            decisions(json!([
                {"action": "retract", "target_id": target, "reason": "contradicted by three later sessions"},
                {"action": "retract", "target_id": target, "reason": format!("contradicted, see {}", Uuid::now_v7())},
                {"action": "retract", "target_id": target, "reason": format!("contradicted by {foreign}")},
            ])),
        )
        .await;
    assert_eq!(counts["rejected"], 3, "no evidence, unknown observation, observation of another space");
    assert_eq!(status_of(&pool, target).await, "active");
    assert_eq!(count(&pool, "SELECT count(*) FROM memory_audit").await, 0);

    let counts = w
        .c
        .apply(
            &packet,
            decisions(json!([{"action": "retract", "target_id": target, "reason": format!("contradicted by obs {evidence}")}])),
        )
        .await;
    assert_eq!(counts["retract"], 1);
    let t = row(&pool, target).await;
    assert_eq!(t["status"], "retracted");
    assert!(!t["valid_to"].is_null());
    assert_eq!(audit_actions(&pool, w.run).await, vec!["fact.retract"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn retract_never_touches_pinned_facts(pool: PgPool) {
    let w = world(pool.clone()).await;
    let evidence = w.obs("e").await;
    let pinned = w.active(FactSpec { pinned: true, ..Default::default() }).await;
    let packet = w.facts_packet(&[pinned]).await;
    let counts =
        w.c.apply(
            &packet,
            decisions(json!([{"action": "retract", "target_id": pinned, "reason": format!("wrong, {evidence}")}])),
        )
        .await;
    assert_eq!(counts["rejected"], 1);
    assert_eq!(status_of(&pool, pinned).await, "active");
}

#[sqlx::test(migrations = "../../migrations")]
async fn promote_scope_moves_a_fact_to_global(pool: PgPool) {
    let w = world(pool.clone()).await;
    let project =
        w.active(FactSpec { predicate: "timezone", value_text: "Europe/Bucharest", ..Default::default() }).await;
    embed_fact(&w.c, project, "Europe/Bucharest").await;
    let blocked = w.active(FactSpec { predicate: "shell", value_text: "zsh", ..Default::default() }).await;
    let _global_shell = w
        .active(FactSpec {
            scope_kind: "global",
            scope_key: "",
            predicate: "shell",
            value_text: "fish",
            ..Default::default()
        })
        .await;
    let packet = w.facts_packet(&[project, blocked]).await;

    let counts =
        w.c.apply(
            &packet,
            decisions(json!([
                {"action": "promote_scope", "target_id": project, "to": "global", "reason": "recurs across 3 projects"},
                {"action": "promote_scope", "target_id": blocked, "to": "global", "reason": "a global shell exists"},
                {"action": "promote_scope", "target_id": blocked, "to": "device", "reason": "no key to derive"},
            ])),
        )
        .await;
    assert_eq!(counts["promote_scope"], 1);
    assert_eq!(counts["rejected"], 2);
    let p = row(&pool, project).await;
    assert_eq!((p["scope_kind"].as_str(), p["scope_key"].as_str()), (Some("global"), Some("")));
    let (kind, key): (String, String) =
        sqlx::query_as("SELECT scope_kind::text, scope_key FROM embeddings WHERE owner_id = $1")
            .bind(project)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((kind.as_str(), key.as_str()), ("global", ""));
    assert_eq!(row(&pool, blocked).await["scope_kind"], "project");
    assert_eq!(audit_actions(&pool, w.run).await, vec!["fact.promote"]);
    // both scopes get a profile re-render: the global profile now lists the timezone
    let global: String =
        sqlx::query_scalar("SELECT body_md FROM profiles WHERE scope_kind = 'global'").fetch_one(&pool).await.unwrap();
    assert!(global.contains("Europe/Bucharest"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn set_confidence(pool: PgPool) {
    let w = world(pool.clone()).await;
    let f = w.active(FactSpec { confidence: 0.5, ..Default::default() }).await;
    let pinned = w.active(FactSpec { pinned: true, predicate: "p", confidence: 0.5, ..Default::default() }).await;
    let retracted = w.active(FactSpec { predicate: "q", ..Default::default() }).await;
    sqlx::query("UPDATE facts SET status = 'retracted' WHERE id = $1").bind(retracted).execute(&pool).await.unwrap();
    let packet = w.facts_packet(&[f, pinned, retracted]).await;
    let counts =
        w.c.apply(
            &packet,
            decisions(json!([
                {"action": "set_confidence", "target_id": f, "confidence": 0.9, "reason": "confirmed on two devices"},
                {"action": "set_confidence", "target_id": pinned, "confidence": 0.1, "reason": "r"},
                {"action": "set_confidence", "target_id": retracted, "confidence": 0.1, "reason": "r"},
            ])),
        )
        .await;
    assert_eq!((counts["set_confidence"], counts["rejected"]), (1, 2));
    assert!((row(&pool, f).await["confidence"].as_f64().unwrap() - 0.9).abs() < 1e-6);
    assert!((row(&pool, pinned).await["confidence"].as_f64().unwrap() - 0.5).abs() < 1e-6);
}

#[sqlx::test(migrations = "../../migrations")]
async fn decisions_never_cross_spaces(pool: PgPool) {
    let w = world(pool.clone()).await;
    let bob = seed(&pool).await;
    let bob_subject = entity(&pool, bob.space_id, "bob").await;
    let theirs_old = fact(&pool, bob.space_id, bob_subject, FactSpec { status: "active", ..Default::default() }).await;
    let theirs_new =
        fact(&pool, bob.space_id, bob_subject, FactSpec { value_text: "emacs", ..Default::default() }).await;
    let mine = w.candidate(FactSpec::default()).await;
    // the foreign ids are even listed in the packet's cluster: the space check stands on its own
    let packet = w.facts_packet(&[mine, theirs_old, theirs_new]).await;
    let evidence = observation(&pool, bob.space_id, ObsSpec::default()).await;

    let counts =
        w.c.apply(
            &packet,
            decisions(json!([
                {"action": "supersede", "target_id": theirs_old, "by_candidate_id": theirs_new, "reason": "r"},
                {"action": "merge_into", "target_id": theirs_old, "candidate_id": mine, "reason": "r"},
                {"action": "retract", "target_id": theirs_old, "reason": format!("e {evidence}")},
                {"action": "promote_scope", "target_id": theirs_old, "to": "global", "reason": "r"},
                {"action": "set_confidence", "target_id": theirs_old, "confidence": 0.1, "reason": "r"},
                {"action": "keep_both", "ids": [theirs_new, mine], "reason": "r"},
            ])),
        )
        .await;
    assert_eq!(counts["rejected"], 6, "{counts:?}");
    assert_eq!(status_of(&pool, theirs_old).await, "active");
    assert_eq!(status_of(&pool, theirs_new).await, "candidate");
    assert_eq!(status_of(&pool, mine).await, "candidate");
    assert_eq!(count(&pool, "SELECT count(*) FROM memory_audit").await, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn ids_outside_the_cluster_are_rejected_and_short_ids_resolve(pool: PgPool) {
    let w = world(pool.clone()).await;
    let old = w.active(FactSpec::default()).await;
    let new = w
        .candidate(FactSpec { value_text: "emacs", conflict: true, supersedes_id: Some(old), ..Default::default() })
        .await;
    let stray = w.candidate(FactSpec { predicate: "stray", ..Default::default() }).await;
    let packet = w.facts_packet(&[old, new]).await;

    let counts =
        w.c.apply(
            &packet,
            decisions(json!([{"action": "set_confidence", "target_id": stray, "confidence": 0.9, "reason": "r"}])),
        )
        .await;
    assert_eq!(counts["rejected"], 1, "a fact of the space but not of the cluster");

    let counts =
        w.c.apply(
            &packet,
            decisions(json!([{"action": "supersede", "target_id": format!("fact#{}", short_id(&old)),
                              "by_candidate_id": short_id(&new), "reason": "short ids as shown in the profile"}])),
        )
        .await;
    assert_eq!(counts["supersede"], 1);
    assert_eq!(status_of(&pool, new).await, "active");
}

#[sqlx::test(migrations = "../../migrations")]
async fn wrong_output_kind_is_an_internal_error(pool: PgPool) {
    let w = world(pool.clone()).await;
    let packet = w.facts_packet(&[]).await;
    let summary = json!({"next_steps": "x", "period": {"start": "2026-10-01", "end": "2026-10-01"}});
    let typed =
        w.c.state
            .schemas
            .validate(memvault_core::schemas::common::OutputSchemaName::ConsolidateSummaryV1, &summary)
            .unwrap();
    let mut tx = w.c.tx().await;
    let err = w.c.state.appliers.apply(&mut tx, &packet, &typed).await.unwrap_err();
    assert_eq!(err.status.as_u16(), 500);
}

// ---------- summary and procedures ----------

#[sqlx::test(migrations = "../../migrations")]
async fn day_summary_is_stored_with_the_packets_period(pool: PgPool) {
    let w = world(pool.clone()).await;
    let packet =
        w.c.packet(
            w.space,
            PacketKind::ConsolidateSummary,
            json!({"run_id": w.run, "grain": "day", "scope_kind": "project", "scope_key": "acme",
                   "period": {"grain": "day", "start": "2026-10-20", "end": "2026-10-20"}, "summaries": []}),
        )
        .await;
    let counts = w
        .c
        .apply(
            &packet,
            json!({"request": "Ship the planner", "learned": "decay needs an incremental clock", "next_steps": "write the revert tests",
                   "period": {"grain": "day", "start": "2026-10-21", "end": "2026-10-21"}}),
        )
        .await;
    assert_eq!(counts["summaries"], 1);
    let (grain, kind, key, start, end, next, tokens): (String, String, String, String, String, String, i32) = sqlx::query_as(
        "SELECT grain, scope_kind::text, scope_key, period_start::text, period_end::text, next_steps, token_estimate
           FROM summaries WHERE space_id = $1",
    )
    .bind(w.space)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((grain.as_str(), kind.as_str(), key.as_str()), ("day", "project", "acme"));
    assert!(start.starts_with("2026-10-20 00:00:00") && end.starts_with("2026-10-21 00:00:00"), "{start} {end}");
    assert_eq!(next, "write the revert tests");
    assert!(tokens > 0);
    assert_eq!(audit_actions(&pool, w.run).await, vec!["sum.create.day"]);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM jobs WHERE type = 'embed' AND payload->>'owner_table' = 'summaries'").await,
        1
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn week_summary_grain(pool: PgPool) {
    let w = world(pool.clone()).await;
    let packet =
        w.c.packet(
            w.space,
            PacketKind::ConsolidateSummary,
            json!({"run_id": w.run, "grain": "week", "scope_kind": "global", "scope_key": "",
                   "period": {"grain": "week", "start": "2026-10-11", "end": "2026-10-17"}}),
        )
        .await;
    w.c.apply(&packet, json!({"next_steps": "continue", "period": {"start": "2026-10-11", "end": "2026-10-17"}})).await;
    let (grain, end): (String, String) =
        sqlx::query_as("SELECT grain, period_end::text FROM summaries").fetch_one(&pool).await.unwrap();
    assert_eq!(grain, "week");
    assert!(end.starts_with("2026-10-18"));
}

fn procedure_inputs(run: Uuid, hints: &[(Uuid, Option<Uuid>)]) -> Value {
    json!({"run_id": run, "group": "project:acme|deploy", "scope_kind": "project", "scope_key": "acme",
           "task_class": "deploy",
           "hints": hints.iter().map(|(h, o)| json!({"id": h, "observation_id": o, "note": "n"})).collect::<Vec<_>>()})
}

fn with_existing(mut inputs: Value, id: Uuid) -> Value {
    inputs["existing_procedure"] = json!({"id": id});
    inputs
}

async fn hint(pool: &PgPool, space: Uuid, session: Option<Uuid>, obs: Option<Uuid>) -> Uuid {
    let id = new_id();
    sqlx::query(
        "INSERT INTO procedure_hints (id, space_id, scope_kind, scope_key, task_class, note, observation_id, session_id)
         VALUES ($1, $2, 'project', 'acme', 'deploy', 'run migrations first', $3, $4)",
    )
    .bind(id)
    .bind(space)
    .bind(obs)
    .bind(session)
    .execute(pool)
    .await
    .unwrap();
    id
}

#[sqlx::test(migrations = "../../migrations")]
async fn procedure_create_then_revise(pool: PgPool) {
    let w = world(pool.clone()).await;
    let o1 = w.obs("deploy 1").await;
    let h1 = hint(&pool, w.space, None, Some(o1)).await;
    let h2 = hint(&pool, w.space, None, None).await;
    let packet =
        w.c.packet(w.space, PacketKind::ConsolidateProcedures, procedure_inputs(w.run, &[(h1, Some(o1)), (h2, None)]))
            .await;

    let counts =
        w.c.apply(
            &packet,
            json!({"name": "Deploy memory-api", "task_class": "deploy", "trigger_text": "when deploying",
                   "body_md": "1. run migrations\n2. roll the Deployment", "confidence": 0.8}),
        )
        .await;
    assert_eq!((counts["procedures_created"], counts["hints_consumed"]), (1, 2));
    let (id, version, status, sources, tokens): (Uuid, i32, String, Vec<Uuid>, i32) = sqlx::query_as(
        "SELECT id, version, status, source_observation_ids, token_estimate FROM procedures WHERE space_id = $1",
    )
    .bind(w.space)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((version, status.as_str(), sources), (1, "active", vec![o1]));
    assert!(tokens > 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM procedure_hints WHERE consumed_at IS NOT NULL").await, 2);
    assert_eq!(audit_actions(&pool, w.run).await, vec!["proc.create"]);

    // a revision creates version 2 and retires version 1
    let run2 = new_id();
    let h3 = hint(&pool, w.space, None, None).await;
    let packet2 = w
        .c
        .packet(w.space, PacketKind::ConsolidateProcedures, with_existing(procedure_inputs(run2, &[(h3, None)]), id))
        .await;
    let counts =
        w.c.apply(
            &packet2,
            json!({"name": "Deploy memory-api", "task_class": "deploy", "trigger_text": "when deploying",
                   "body_md": "1. run migrations\n2. roll the Deployment\n3. check /health", "supersedes_id": id}),
        )
        .await;
    assert_eq!(counts["procedures_revised"], 1);
    let rows: Vec<(i32, String, Option<Uuid>)> =
        sqlx::query_as("SELECT version, status, supersedes_id FROM procedures ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![(1, "retired".into(), None), (2, "active".into(), Some(id))]);
    assert_eq!(audit_actions(&pool, run2).await, vec!["proc.retire", "proc.revise"]);

    // revising something that is not an active procedure of the space is rejected
    let packet3 =
        w.c.packet(w.space, PacketKind::ConsolidateProcedures, with_existing(procedure_inputs(new_id(), &[]), id))
            .await;
    let counts =
        w.c.apply(
            &packet3,
            json!({"name": "n", "task_class": "deploy", "trigger_text": "t", "body_md": "b", "supersedes_id": id}),
        )
        .await;
    assert_eq!(counts["rejected"], 1, "version 1 is retired already");
    assert_eq!(count(&pool, "SELECT count(*) FROM procedures").await, 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn procedure_skip_consumes_hints_and_creates_nothing(pool: PgPool) {
    let w = world(pool.clone()).await;
    let h = hint(&pool, w.space, None, None).await;
    let packet = w.c.packet(w.space, PacketKind::ConsolidateProcedures, procedure_inputs(w.run, &[(h, None)])).await;
    let counts = w.c.apply(&packet, json!({"skip": true, "skip_reason": "not a procedure"})).await;
    assert_eq!((counts["skipped"], counts["hints_consumed"]), (1, 1));
    assert_eq!(count(&pool, "SELECT count(*) FROM procedures").await, 0);
}

async fn make_procedure(pool: &PgPool, space: Uuid, scope_key: &str, task_class: &str) -> Uuid {
    let id = new_id();
    sqlx::query(
        "INSERT INTO procedures (id, space_id, scope_kind, scope_key, name, task_class, trigger_text, body_md, version, token_estimate)
         VALUES ($1, $2, 'project', $3, 'p', $4, 't', 'b', 1, 1)",
    )
    .bind(id)
    .bind(space)
    .bind(scope_key)
    .bind(task_class)
    .execute(pool)
    .await
    .unwrap();
    id
}

#[sqlx::test(migrations = "../../migrations")]
async fn procedure_revision_must_target_the_packets_procedure(pool: PgPool) {
    let w = world(pool.clone()).await;
    let mine = make_procedure(&pool, w.space, "acme", "deploy").await;
    let foreign = make_procedure(&pool, w.space, "other", "release").await;
    let h = hint(&pool, w.space, None, None).await;
    let packet = w
        .c
        .packet(w.space, PacketKind::ConsolidateProcedures, with_existing(procedure_inputs(w.run, &[(h, None)]), mine))
        .await;
    let counts =
        w.c.apply(
            &packet,
            json!({"name": "n", "task_class": "deploy", "trigger_text": "t", "body_md": "b", "supersedes_id": foreign}),
        )
        .await;
    assert_eq!(counts["rejected"], 1);
    assert_eq!(count(&pool, "SELECT count(*) FROM procedures WHERE status = 'active'").await, 2);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM procedure_hints WHERE consumed_at IS NOT NULL").await,
        0,
        "hints survive a rejection"
    );

    // supersedes_id left out: the output still revises the carried procedure; class comes from the packet
    let counts =
        w.c.apply(&packet, json!({"name": "n", "task_class": "hallucinated", "trigger_text": "t", "body_md": "b"}))
            .await;
    assert_eq!(counts["procedures_revised"], 1);
    let rows: Vec<(String, i32, String)> =
        sqlx::query_as("SELECT status, version, task_class FROM procedures WHERE scope_key = 'acme' ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![("retired".into(), 1, "deploy".into()), ("active".into(), 2, "deploy".into())]);
    assert_eq!(count(&pool, "SELECT count(*) FROM procedure_hints WHERE consumed_at IS NOT NULL").await, 1);

    // a packet without a carried procedure cannot retire anything
    let packet = w.c.packet(w.space, PacketKind::ConsolidateProcedures, procedure_inputs(new_id(), &[])).await;
    let counts =
        w.c.apply(
            &packet,
            json!({"name": "n", "task_class": "deploy", "trigger_text": "t", "body_md": "b", "supersedes_id": foreign}),
        )
        .await;
    assert_eq!(counts["rejected"], 1);
    assert_eq!(text(&pool, "SELECT status FROM procedures WHERE id = $1", foreign).await, "active");
}
