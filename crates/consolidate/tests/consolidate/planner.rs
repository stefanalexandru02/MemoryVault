//! The planner (spec 12.1): clusters, auto-activation, decay and archival, summaries, procedures,
//! profiles, denormalization.

use chrono::{DateTime, Duration, TimeZone, Utc};
use memvault_consolidate::planner::clusters::COSINE_THRESHOLD;
use memvault_consolidate::planner::decay::{HALF_LIFE_FACT_DAYS, decayed_importance, half_life_days};
use memvault_consolidate::planner::{run_planner, run_planner_at};
use memvault_core::ids::new_id;
use memvault_core::schemas::common::{PacketKind, PacketStatus};
use memvault_core::schemas::work::PacketRecord;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::*;

struct World {
    c: Ctx,
    space: Uuid,
    client: Uuid,
    subject: Uuid,
}

async fn world(pool: PgPool) -> World {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    World { c, space: alice.space_id, client: alice.client_id, subject }
}

impl World {
    async fn fact(&self, spec: FactSpec) -> Uuid {
        fact(&self.c.pool, self.space, self.subject, spec).await
    }

    async fn packets(&self, kind: PacketKind) -> Vec<PacketRecord> {
        sqlx::query_as("SELECT * FROM work_packets WHERE space_id = $1 AND kind = $2 ORDER BY created_at, id")
            .bind(self.space)
            .bind(kind)
            .fetch_all(&self.c.pool)
            .await
            .unwrap()
    }
}

fn fact_ids(p: &PacketRecord) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> =
        p.inputs["fact_ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().parse().unwrap()).collect();
    ids.sort();
    ids
}

fn sorted(mut v: Vec<Uuid>) -> Vec<Uuid> {
    v.sort();
    v
}

// ---------- fact clusters ----------

#[sqlx::test(migrations = "../../migrations")]
async fn a_conflict_becomes_one_facts_packet(pool: PgPool) {
    let w = world(pool.clone()).await;
    let evidence = observation(&pool, w.space, ObsSpec { title: "switched to emacs", ..Default::default() }).await;
    let active = w.fact(FactSpec { status: "active", value_text: "vim", ..Default::default() }).await;
    let candidate = w
        .fact(FactSpec {
            value_text: "emacs",
            conflict: true,
            supersedes_id: Some(active),
            evidence: vec![evidence],
            ..Default::default()
        })
        .await;
    let unrelated = w.fact(FactSpec { predicate: "timezone", value_text: "UTC", ..Default::default() }).await;
    sqlx::query("INSERT INTO profiles (space_id, scope_kind, scope_key, body_md, token_estimate, facts_version) VALUES ($1, 'project', 'acme', 'PROFILE-TEXT', 3, 0)")
        .bind(w.space)
        .execute(&pool)
        .await
        .unwrap();

    let out = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert_eq!(out.packets_created["consolidate_facts"], 1, "{out:?}");

    let packets = w.packets(PacketKind::ConsolidateFacts).await;
    assert_eq!(packets.len(), 1);
    let p = &packets[0];
    assert_eq!(fact_ids(p), sorted(vec![active, candidate]));
    assert_eq!(p.inputs["run_id"], json!(out.run_id));
    assert_eq!(p.prompt_ref, "consolidate.facts@v1");
    assert_eq!(p.model_hint, "opus");
    assert_eq!(p.status, PacketStatus::Pending);
    assert_eq!(p.output_schema.as_str(), "consolidate.facts.v1");
    assert_eq!(p.inputs["profile_md"], "PROFILE-TEXT");
    let facts = p.inputs["facts"].as_array().unwrap();
    assert_eq!(facts[0]["id"], json!(candidate), "candidates first");
    assert_eq!(facts[0]["evidence"][0]["title"], "switched to emacs");
    assert_eq!(facts[1]["status"], "active");
    assert_eq!(facts[0]["subject"]["name"], "alex");
    assert_eq!(facts[0]["scope"], "project:acme");

    // the unrelated candidate is lone and old enough: auto-activated, not clustered
    assert_eq!(out.actions["auto_activate"], 1);
    assert_eq!(status_of(&pool, unrelated).await, "active");
    assert_eq!(status_of(&pool, candidate).await, "candidate", "a conflicting candidate waits for the model");

    // the prompt renders with the packet inputs (strict undefined)
    let spec = w.c.state.prompts.get("consolidate.facts", None).unwrap();
    let rendered = w.c.state.prompts.render(spec, &p.inputs).unwrap();
    assert!(rendered.user.contains("switched to emacs") && rendered.user.contains("PROFILE-TEXT"));

    // running again plans nothing new: the cluster is in an open packet
    let again = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert!(again.packets_created.is_empty(), "{again:?}");
    assert_eq!(w.packets(PacketKind::ConsolidateFacts).await.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn candidates_on_one_key_cluster_even_without_a_conflict_flag(pool: PgPool) {
    let w = world(pool.clone()).await;
    let a = w.fact(FactSpec { value_text: "vim", ..Default::default() }).await;
    let b = w.fact(FactSpec { value_text: "emacs", ..Default::default() }).await;
    let other_scope = w.fact(FactSpec { scope_key: "elsewhere", value_text: "nano", ..Default::default() }).await;
    let out = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert_eq!(out.packets_created["consolidate_facts"], 1);
    assert_eq!(fact_ids(&w.packets(PacketKind::ConsolidateFacts).await[0]), sorted(vec![a, b]));
    assert_eq!(status_of(&pool, other_scope).await, "active", "the lone one is activated");
    assert_eq!(status_of(&pool, a).await, "candidate");
}

#[sqlx::test(migrations = "../../migrations")]
async fn similar_values_cluster_by_embedding_cosine(pool: PgPool) {
    let w = world(pool.clone()).await;
    let active_text = "uses neovim as the main code editor";
    let similar_text = "uses neovim as the main code editor daily";
    let far_text = "lives in Bucharest Romania";
    // the premise of the test: the fake embedder puts these on the right sides of the threshold
    let cos = |a: &str, b: &str| -> f64 {
        let (x, y) = (w.c.tei.vector(a), w.c.tei.vector(b));
        x.iter().zip(&y).map(|(p, q)| f64::from(p * q)).sum()
    };
    assert!(cos(active_text, similar_text) > COSINE_THRESHOLD);
    assert!(cos(active_text, far_text) < COSINE_THRESHOLD);

    let active = w
        .fact(FactSpec { status: "active", predicate: "uses.editor", value_text: active_text, ..Default::default() })
        .await;
    embed_fact(&w.c, active, active_text).await;
    // different predicate, so only the embedding links them; the candidate has no stored embedding
    // yet and is embedded on the fly
    let similar =
        w.fact(FactSpec { predicate: "prefers.editor", value_text: similar_text, ..Default::default() }).await;
    let far = w.fact(FactSpec { predicate: "lives.in", value_text: far_text, ..Default::default() }).await;
    embed_fact(&w.c, far, far_text).await;
    // an active fact of another space with the same text never joins
    let bob = seed(&pool).await;
    let bob_subject = entity(&pool, bob.space_id, "bob").await;
    let bobs = fact(
        &pool,
        bob.space_id,
        bob_subject,
        FactSpec { status: "active", predicate: "x", value_text: similar_text, ..Default::default() },
    )
    .await;
    embed_fact(&w.c, bobs, similar_text).await;

    let out = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert_eq!(out.packets_created["consolidate_facts"], 1, "{out:?}");
    assert_eq!(fact_ids(&w.packets(PacketKind::ConsolidateFacts).await[0]), sorted(vec![active, similar]));
    assert_eq!(status_of(&pool, far).await, "active", "no neighbour: auto-activated");
    assert_eq!(status_of(&pool, bobs).await, "active");
}

#[sqlx::test(migrations = "../../migrations")]
async fn near_duplicate_candidates_cluster_together(pool: PgPool) {
    let w = world(pool.clone()).await;
    let t1 = "deploys the memory api with helm every friday";
    let t2 = "deploys the memory api with helm every friday evening";
    let a = w.fact(FactSpec { predicate: "deploy.a", value_text: t1, ..Default::default() }).await;
    let b = w.fact(FactSpec { predicate: "deploy.b", value_text: t2, ..Default::default() }).await;
    embed_fact(&w.c, a, t1).await;
    embed_fact(&w.c, b, t2).await;
    let out = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert_eq!(out.packets_created["consolidate_facts"], 1);
    assert_eq!(fact_ids(&w.packets(PacketKind::ConsolidateFacts).await[0]), sorted(vec![a, b]));
}

#[sqlx::test(migrations = "../../migrations")]
async fn lone_candidates_are_auto_activated_only_when_old_confident_and_unconflicted(pool: PgPool) {
    let w = world(pool.clone()).await;
    let ok = w.fact(FactSpec { predicate: "a", age: Duration::hours(25), confidence: 0.6, ..Default::default() }).await;
    let young =
        w.fact(FactSpec { predicate: "b", age: Duration::hours(23), confidence: 0.9, ..Default::default() }).await;
    let weak =
        w.fact(FactSpec { predicate: "c", age: Duration::days(5), confidence: 0.59, ..Default::default() }).await;
    let conflicted =
        w.fact(FactSpec { predicate: "d", age: Duration::days(5), conflict: true, ..Default::default() }).await;
    let out = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert_eq!(out.actions["auto_activate"], 1);
    assert!(out.packets_created.is_empty());
    assert_eq!(status_of(&pool, ok).await, "active");
    for id in [young, weak, conflicted] {
        assert_eq!(status_of(&pool, id).await, "candidate");
    }
    // audited fact.create by the planner under the run id, with before/after
    let (action, actor, before, after): (String, String, Value, Value) =
        sqlx::query_as("SELECT action, actor, before, after FROM memory_audit WHERE run_id = $1 AND target_id = $2")
            .bind(out.run_id)
            .bind(ok)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((action.as_str(), actor.as_str()), ("fact.create", "planner"));
    assert_eq!((before["status"].as_str(), after["status"].as_str()), (Some("candidate"), Some("active")));
    // it shows up in the scope's profile
    let profile: String = sqlx::query_scalar("SELECT body_md FROM profiles WHERE space_id = $1")
        .bind(w.space)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(profile.contains("- alex: a = vim"), "{profile}");
    assert_eq!(out.actions["profiles_rendered"], 1);
}

// ---------- decay and archival ----------

fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
}

async fn archived(pool: &PgPool, id: Uuid) -> bool {
    sqlx::query_scalar("SELECT archived_at IS NOT NULL FROM observations WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn importance(pool: &PgPool, table: &str, id: Uuid) -> f64 {
    sqlx::query_scalar(&format!("SELECT importance::float8 FROM {table} WHERE id = $1"))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn importance_decays_by_half_life(pool: PgPool) {
    let w = world(pool.clone()).await;
    let now = at(2026, 10, 21, 3);
    let mut obs = Vec::new();
    for t in ["change", "discovery", "decision", "bugfix", "feature"] {
        let id = observation(
            &pool,
            w.space,
            ObsSpec { obs_type: t, importance: 0.8, created_at: now - Duration::days(30), ..Default::default() },
        )
        .await;
        obs.push((t, id));
    }
    let fact_id =
        w.fact(FactSpec { status: "active", importance: 0.6, age: Duration::days(0), ..Default::default() }).await;
    sqlx::query("UPDATE facts SET first_seen_at = $2 WHERE id = $1")
        .bind(fact_id)
        .bind(now - Duration::days(365))
        .execute(&pool)
        .await
        .unwrap();
    let pinned = w
        .fact(FactSpec { status: "active", importance: 0.6, pinned: true, predicate: "p", ..Default::default() })
        .await;
    sqlx::query("UPDATE facts SET first_seen_at = $2 WHERE id = $1")
        .bind(pinned)
        .bind(now - Duration::days(365))
        .execute(&pool)
        .await
        .unwrap();
    let floored = w.fact(FactSpec { status: "active", importance: 0.12, predicate: "f", ..Default::default() }).await;
    sqlx::query("UPDATE facts SET first_seen_at = $2 WHERE id = $1")
        .bind(floored)
        .bind(now - Duration::days(3650))
        .execute(&pool)
        .await
        .unwrap();

    let out = run_planner_at(&w.c.state, Some(w.space), now).await.unwrap();
    assert_eq!(out.actions["decay_observations"], 5);

    for (t, id) in &obs {
        let want = decayed_importance(0.8, 30.0, half_life_days(t), 0.0);
        let got = importance(&pool, "observations", *id).await;
        assert!((got - want).abs() < 1e-5, "{t}: {got} vs {want}");
    }
    assert!((importance(&pool, "observations", obs[0].1).await - 0.8 * 0.25_f64.powf(30.0 / 28.0)).abs() < 1e-5);
    let got = importance(&pool, "facts", fact_id).await;
    assert!(
        (got - decayed_importance(0.6, 365.0, HALF_LIFE_FACT_DAYS, 0.1)).abs() < 1e-5 && (got - 0.3).abs() < 1e-5,
        "{got}"
    );
    assert!((importance(&pool, "facts", pinned).await - 0.6).abs() < 1e-6, "pinned facts do not decay");
    assert!((importance(&pool, "facts", floored).await - 0.1).abs() < 1e-6, "facts floor at 0.1");

    // a second pass right after decays nothing more; a later one decays only the time since
    run_planner_at(&w.c.state, Some(w.space), now + Duration::hours(1)).await.unwrap();
    assert!(
        (importance(&pool, "observations", obs[0].1).await - decayed_importance(0.8, 30.0, 14.0, 0.0)).abs() < 1e-5
    );
    run_planner_at(&w.c.state, Some(w.space), now + Duration::days(14)).await.unwrap();
    let want = decayed_importance(0.8, 44.0, 14.0, 0.0);
    assert!((importance(&pool, "observations", obs[0].1).await - want).abs() < 1e-5);
}

#[sqlx::test(migrations = "../../migrations")]
async fn faded_old_observations_are_archived_with_an_audit_row(pool: PgPool) {
    let w = world(pool.clone()).await;
    let now = at(2026, 10, 21, 3);
    let old = |importance: f64, days: i64, t: &'static str| ObsSpec {
        obs_type: t,
        importance,
        created_at: now - Duration::days(days),
        ..Default::default()
    };
    let faded = observation(&pool, w.space, old(0.04, 61, "decision")).await;
    let recent = observation(&pool, w.space, old(0.04, 10, "decision")).await;
    let important = observation(&pool, w.space, old(0.5, 61, "decision")).await;
    let decays_below = observation(&pool, w.space, old(0.2, 120, "change")).await;

    let out = run_planner_at(&w.c.state, Some(w.space), now).await.unwrap();
    assert_eq!(out.actions["obs_archive"], 2, "{out:?}");
    assert!(archived(&pool, faded).await && archived(&pool, decays_below).await);
    assert!(!archived(&pool, recent).await && !archived(&pool, important).await);
    let (action, before, after): (String, Value, Value) =
        sqlx::query_as("SELECT action, before, after FROM memory_audit WHERE run_id = $1 AND target_id = $2")
            .bind(out.run_id)
            .bind(faded)
            .fetch_one(&w.c.pool)
            .await
            .unwrap();
    assert_eq!(action, "obs.archive");
    assert!(before["archived_at"].is_null() && !after["archived_at"].is_null());
}

// ---------- summaries ----------

async fn session_summary(pool: &PgPool, space: Uuid, scope_key: &str, at: DateTime<Utc>) -> Uuid {
    let id = new_id();
    sqlx::query(
        "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, request, next_steps, extracted_by, created_at)
         VALUES ($1, $2, 'project', $3, 'session', 'req', 'next', '{}'::jsonb, $4)",
    )
    .bind(id)
    .bind(space)
    .bind(scope_key)
    .bind(at)
    .execute(pool)
    .await
    .unwrap();
    id
}

#[sqlx::test(migrations = "../../migrations")]
async fn day_summaries_are_planned_for_finished_days_with_two_summaries(pool: PgPool) {
    let w = world(pool.clone()).await;
    let now = at(2026, 10, 21, 3); // a Wednesday
    let (a, b) = (
        session_summary(&pool, w.space, "acme", at(2026, 10, 20, 9)).await,
        session_summary(&pool, w.space, "acme", at(2026, 10, 20, 15)).await,
    );
    session_summary(&pool, w.space, "solo", at(2026, 10, 20, 10)).await; // one is not enough
    session_summary(&pool, w.space, "acme", at(2026, 10, 21, 1)).await; // today is not over
    session_summary(&pool, w.space, "acme", at(2026, 10, 21, 2)).await;
    let done_a = session_summary(&pool, w.space, "done", at(2026, 10, 19, 9)).await;
    session_summary(&pool, w.space, "done", at(2026, 10, 19, 10)).await;
    sqlx::query(
        "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, period_start, period_end, next_steps, extracted_by)
         VALUES ($1, $2, 'project', 'done', 'day', $3, $4, 'n', '{}'::jsonb)",
    )
    .bind(new_id())
    .bind(w.space)
    .bind(at(2026, 10, 19, 0))
    .bind(at(2026, 10, 20, 0))
    .execute(&pool)
    .await
    .unwrap();
    let _ = done_a;

    let out = run_planner_at(&w.c.state, Some(w.space), now).await.unwrap();
    assert_eq!(out.packets_created["consolidate_summary"], 1, "{out:?}");
    let p = &w.packets(PacketKind::ConsolidateSummary).await[0];
    assert_eq!(p.inputs["grain"], "day");
    assert_eq!((p.inputs["scope_kind"].as_str(), p.inputs["scope_key"].as_str()), (Some("project"), Some("acme")));
    assert_eq!(p.inputs["period"], json!({"grain": "day", "start": "2026-10-20", "end": "2026-10-20"}));
    let ids: Vec<&str> = p.inputs["summaries"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![a.to_string(), b.to_string()]);
    assert_eq!(p.prompt_ref, "consolidate.summary@v1");
    let spec = w.c.state.prompts.get("consolidate.summary", None).unwrap();
    assert!(w.c.state.prompts.render(spec, &p.inputs).unwrap().user.contains("2026-10-20"));

    let again = run_planner_at(&w.c.state, Some(w.space), now).await.unwrap();
    assert!(again.packets_created.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn week_summaries_are_planned_on_sundays_only(pool: PgPool) {
    let w = world(pool.clone()).await;
    for day in [19, 20, 22] {
        sqlx::query(
            "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, period_start, period_end, next_steps, extracted_by)
             VALUES ($1, $2, 'project', 'acme', 'day', $3, $4, 'n', '{}'::jsonb)",
        )
        .bind(new_id())
        .bind(w.space)
        .bind(at(2026, 10, day, 0))
        .bind(at(2026, 10, day + 1, 0))
        .execute(&pool)
        .await
        .unwrap();
    }
    session_summary(&pool, w.space, "sessions-only", at(2026, 10, 19, 9)).await;
    session_summary(&pool, w.space, "sessions-only", at(2026, 10, 22, 9)).await;
    session_summary(&pool, w.space, "thin", at(2026, 10, 22, 9)).await;

    run_planner_at(&w.c.state, Some(w.space), at(2026, 10, 24, 3)).await.unwrap(); // a Saturday
    let before = w.packets(PacketKind::ConsolidateSummary).await;
    assert!(before.iter().all(|p| p.inputs["grain"] == "day"), "no week packet before Sunday");

    let sunday = run_planner_at(&w.c.state, Some(w.space), at(2026, 10, 25, 3)).await.unwrap();
    let weeks: Vec<PacketRecord> =
        w.packets(PacketKind::ConsolidateSummary).await.into_iter().filter(|p| p.inputs["grain"] == "week").collect();
    assert_eq!(
        weeks.len(),
        2,
        "acme from day summaries, sessions-only from session summaries; thin has too little: {sunday:?}"
    );
    let acme = weeks.iter().find(|p| p.inputs["scope_key"] == "acme").unwrap();
    assert_eq!(acme.inputs["period"], json!({"grain": "week", "start": "2026-10-18", "end": "2026-10-24"}));
    assert_eq!(acme.inputs["summaries"].as_array().unwrap().len(), 3);
    assert!(acme.inputs["summaries"][0]["id"].is_string());
    let again = run_planner_at(&w.c.state, Some(w.space), at(2026, 10, 25, 5)).await.unwrap();
    assert!(!again.packets_created.contains_key("consolidate_summary"), "{again:?}");
    assert_eq!(
        w.packets(PacketKind::ConsolidateSummary).await.iter().filter(|p| p.inputs["grain"] == "week").count(),
        2
    );
}

// ---------- procedures ----------

async fn hint(pool: &PgPool, space: Uuid, task: &str, session: Option<Uuid>, obs: Option<Uuid>) -> Uuid {
    let id = new_id();
    sqlx::query(
        "INSERT INTO procedure_hints (id, space_id, scope_kind, scope_key, task_class, note, observation_id, session_id)
         VALUES ($1, $2, 'project', 'acme', $3, 'note', $4, $5)",
    )
    .bind(id)
    .bind(space)
    .bind(task)
    .bind(obs)
    .bind(session)
    .execute(pool)
    .await
    .unwrap();
    id
}

#[sqlx::test(migrations = "../../migrations")]
async fn recurring_hints_become_a_procedures_packet(pool: PgPool) {
    let w = world(pool.clone()).await;
    let (s1, s2) = (session(&pool, w.space, w.client, "acme").await, session(&pool, w.space, w.client, "acme").await);
    let obs = observation(&pool, w.space, ObsSpec { title: "deployed the api", ..Default::default() }).await;
    let hints = [
        hint(&pool, w.space, "deploy", Some(s1), Some(obs)).await,
        hint(&pool, w.space, "deploy", Some(s1), None).await,
        hint(&pool, w.space, "deploy", Some(s2), None).await,
    ];
    // 3 hints in one session, and 2 hints in two sessions: neither qualifies
    for _ in 0..3 {
        hint(&pool, w.space, "lint", Some(s1), None).await;
    }
    hint(&pool, w.space, "test", Some(s1), None).await;
    hint(&pool, w.space, "test", Some(s2), None).await;
    let existing = new_id();
    sqlx::query("INSERT INTO procedures (id, space_id, scope_kind, scope_key, name, task_class, trigger_text, body_md) VALUES ($1, $2, 'project', 'acme', 'Deploy', 'deploy', 't', 'b')")
        .bind(existing)
        .bind(w.space)
        .execute(&pool)
        .await
        .unwrap();

    let out = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert_eq!(out.packets_created["consolidate_procedures"], 1, "{out:?}");
    let p = &w.packets(PacketKind::ConsolidateProcedures).await[0];
    assert_eq!(p.inputs["task_class"], "deploy");
    assert_eq!(p.inputs["run_id"], json!(out.run_id));
    let got: Vec<Uuid> =
        p.inputs["hints"].as_array().unwrap().iter().map(|h| h["id"].as_str().unwrap().parse().unwrap()).collect();
    assert_eq!(sorted(got), sorted(hints.to_vec()));
    assert_eq!(p.inputs["observations"][0]["title"], "deployed the api");
    assert_eq!(p.inputs["existing_procedure"]["id"], json!(existing));
    let spec = w.c.state.prompts.get("consolidate.procedures", None).unwrap();
    assert!(w.c.state.prompts.render(spec, &p.inputs).unwrap().user.contains("Existing procedure"));

    // not planned twice while it is open
    let again = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert!(again.packets_created.is_empty());
    hint(&pool, w.space, "deploy", Some(s2), None).await;
    let more = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert!(more.packets_created.is_empty(), "the group already has an open packet");
}

// ---------- profiles and denormalization ----------

#[sqlx::test(migrations = "../../migrations")]
async fn profiles_follow_the_active_facts(pool: PgPool) {
    let w = world(pool.clone()).await;
    let pinned = w
        .fact(FactSpec {
            status: "active",
            pinned: true,
            predicate: "address.me",
            value_text: "Dr Alex",
            importance: 0.1,
            ..Default::default()
        })
        .await;
    let strong = w
        .fact(FactSpec {
            status: "active",
            predicate: "prefers.editor",
            value_text: "vim",
            importance: 0.9,
            confidence: 0.9,
            ..Default::default()
        })
        .await;
    let weak = w
        .fact(FactSpec {
            status: "active",
            predicate: "shell",
            value_text: "zsh",
            importance: 0.2,
            confidence: 0.5,
            ..Default::default()
        })
        .await;
    w.fact(FactSpec {
        status: "active",
        scope_kind: "global",
        scope_key: "",
        predicate: "timezone",
        value_text: "UTC",
        ..Default::default()
    })
    .await;
    w.fact(FactSpec { status: "retracted", predicate: "old", value_text: "retracted-value", ..Default::default() })
        .await;

    let out = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert_eq!(out.actions["profiles_rendered"], 2);
    let project: (String, i32) = sqlx::query_as(
        "SELECT body_md, token_estimate FROM profiles WHERE scope_kind = 'project' AND scope_key = 'acme'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let lines: Vec<&str> = project.0.lines().collect();
    assert_eq!(lines.len(), 3, "{}", project.0);
    assert!(
        lines[0].contains("Dr Alex") && lines[0].contains(&format!("fact#{}", memvault_core::ids::short_id(&pinned))),
        "pinned first: {}",
        lines[0]
    );
    assert!(lines[1].contains("vim") && lines[2].contains("zsh"));
    assert!(lines[1].starts_with("- alex: prefers.editor = vim [fact#") && lines[1].contains(" · 0.90 · "));
    assert!(!project.0.contains("retracted-value"));
    assert!(project.1 <= 400);
    let _ = (strong, weak);

    // nothing changed: nothing re-rendered; a change re-renders only its scope
    let again = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert!(!again.actions.contains_key("profiles_rendered"), "{again:?}");
    sqlx::query("UPDATE facts SET value_text = 'emacs' WHERE id = $1").bind(strong).execute(&pool).await.unwrap();
    let after = run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert_eq!(after.actions["profiles_rendered"], 1);
    let body: String =
        sqlx::query_scalar("SELECT body_md FROM profiles WHERE scope_kind = 'project'").fetch_one(&pool).await.unwrap();
    assert!(body.contains("emacs") && !body.contains("= vim"));

    // a scope that lost its last active fact loses its profile
    sqlx::query("UPDATE facts SET status = 'retracted' WHERE scope_kind = 'global'").execute(&pool).await.unwrap();
    run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM profiles WHERE scope_kind = 'global'").await, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn profile_is_cut_at_400_tokens(pool: PgPool) {
    let w = world(pool.clone()).await;
    let long: &'static str = Box::leak("x".repeat(300).into_boxed_str());
    for i in 0..20 {
        let predicate: &'static str = Box::leak(format!("p{i}").into_boxed_str());
        w.fact(FactSpec { status: "active", predicate, value_text: long, ..Default::default() }).await;
    }
    run_planner(&w.c.state, Some(w.space)).await.unwrap();
    let (body, tokens): (String, i32) =
        sqlx::query_as("SELECT body_md, token_estimate FROM profiles").fetch_one(&pool).await.unwrap();
    assert!(tokens <= 400 && tokens > 300, "{tokens}");
    assert!(body.lines().count() < 20);
}

#[sqlx::test(migrations = "../../migrations")]
async fn learned_on_clients_come_from_the_evidence_chain(pool: PgPool) {
    let w = world(pool.clone()).await;
    let desktop = memvault_core::test_support::seed_identity(&pool, memvault_core::auth::TokenKind::Device).await;
    // a second client of the same user would need the same user id; use two clients of two users in
    // one shared space instead: evidence from alice's laptop and bob's desktop
    let shared = memvault_core::test_support::seed_shared_space(
        &pool,
        "team",
        desktop.user_id,
        &[(desktop.user_id, memvault_core::schemas::common::MembershipRole::Owner)],
    )
    .await;
    let s_shared = entity(&pool, shared, "team").await;
    let sess_a = session(&pool, shared, w.client, "acme").await;
    let sess_b = session(&pool, shared, desktop.client_id, "acme").await;
    let o_a = observation(&pool, shared, ObsSpec { session_id: Some(sess_a), ..Default::default() }).await;
    let o_b = observation(&pool, shared, ObsSpec { session_id: Some(sess_b), ..Default::default() }).await;
    let o_none = observation(&pool, shared, ObsSpec::default()).await;
    let learned = fact(
        &pool,
        shared,
        s_shared,
        FactSpec { status: "active", evidence: vec![o_a, o_b, o_none], ..Default::default() },
    )
    .await;
    let untouched = fact(
        &pool,
        shared,
        s_shared,
        FactSpec { status: "active", predicate: "explicit", evidence: vec![o_none], ..Default::default() },
    )
    .await;
    sqlx::query("UPDATE facts SET learned_on_client_ids = ARRAY[$2]::uuid[] WHERE id = $1")
        .bind(untouched)
        .bind(w.client)
        .execute(&pool)
        .await
        .unwrap();

    let out = run_planner(&w.c.state, Some(shared)).await.unwrap();
    assert_eq!(out.actions["learned_on_refreshed"], 1);
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT unnest(learned_on_client_ids) FROM facts WHERE id = $1")
        .bind(learned)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(sorted(ids), sorted(vec![w.client, desktop.client_id]));
    let kept: Vec<Uuid> = sqlx::query_scalar("SELECT unnest(learned_on_client_ids) FROM facts WHERE id = $1")
        .bind(untouched)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(kept, vec![w.client], "a fact whose evidence has no session keeps its clients");
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_space_is_planned_independently_under_one_run_id(pool: PgPool) {
    let w = world(pool.clone()).await;
    let bob = seed(&pool).await;
    let bob_subject = entity(&pool, bob.space_id, "bob").await;
    let mine = w.fact(FactSpec::default()).await;
    let theirs = fact(&pool, bob.space_id, bob_subject, FactSpec::default()).await;
    let out = run_planner(&w.c.state, None).await.unwrap();
    assert_eq!(out.actions["auto_activate"], 2);
    assert!(!out.actions.contains_key("space_errors"));
    assert_eq!(status_of(&pool, mine).await, "active");
    assert_eq!(status_of(&pool, theirs).await, "active");
    let runs: Vec<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT run_id FROM memory_audit").fetch_all(&pool).await.unwrap();
    assert_eq!(runs, vec![out.run_id]);
    // one space asked for: the other is left alone
    let c = fact(&pool, bob.space_id, bob_subject, FactSpec { predicate: "later", ..Default::default() }).await;
    run_planner(&w.c.state, Some(w.space)).await.unwrap();
    assert_eq!(status_of(&pool, c).await, "candidate");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_cluster_is_planned_again_after_its_packet_ended_without_settling_it(pool: PgPool) {
    let w = world(pool.clone()).await;
    let active = w.fact(FactSpec { status: "active", value_text: "vim", ..Default::default() }).await;
    let candidate = w
        .fact(FactSpec { value_text: "emacs", conflict: true, supersedes_id: Some(active), ..Default::default() })
        .await;

    run_planner(&w.c.state, Some(w.space)).await.unwrap();
    for generation in 1..=3 {
        let packets = w.packets(PacketKind::ConsolidateFacts).await;
        assert_eq!(packets.len(), generation);
        // the last packet ends done (a no-op result) or failed; the candidate stays a candidate
        let status = if generation == 2 { "failed" } else { "done" };
        sqlx::query("UPDATE work_packets SET status = $2 WHERE id = $1")
            .bind(packets.last().unwrap().id)
            .bind(status)
            .execute(&pool)
            .await
            .unwrap();
        let again = run_planner(&w.c.state, Some(w.space)).await.unwrap();
        if generation < 3 {
            assert_eq!(again.packets_created["consolidate_facts"], 1, "{again:?}");
        } else {
            assert!(again.packets_created.is_empty(), "capped after three finished packets");
        }
    }
    assert_eq!(w.packets(PacketKind::ConsolidateFacts).await.len(), 3);
    assert_eq!(status_of(&pool, candidate).await, "candidate");
}
