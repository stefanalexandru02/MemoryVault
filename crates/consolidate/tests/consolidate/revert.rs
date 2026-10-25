//! Revert (spec 12.4): a run's audit rows replayed backwards.

use chrono::{Duration, Utc};
use memvault_consolidate::planner::run_planner;
use memvault_consolidate::revert::revert_run;
use memvault_core::ids::new_id;
use memvault_core::schemas::common::PacketKind;
use memvault_core::schemas::work::PacketRecord;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::*;

async fn revert(c: &Ctx, run: Uuid) -> Result<memvault_core::schemas::admin::RevertResponse, memvault_core::ApiError> {
    let mut tx = c.tx().await;
    let out = revert_run(&mut tx, run, "test").await?;
    tx.commit().await.unwrap();
    Ok(out)
}

async fn fact_state(pool: &PgPool, id: Uuid) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object('status', status, 'supersedes_id', supersedes_id, 'superseded_by', superseded_by,
                                   'valid_from', valid_from, 'valid_to', valid_to, 'conflict', conflict,
                                   'confidence', confidence, 'confirmations', confirmations,
                                   'scope_kind', scope_kind, 'scope_key', scope_key,
                                   'evidence', evidence_observation_ids)
           FROM facts WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_planner_run_and_its_packet_decisions_revert_together(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let space = alice.space_id;
    let subject = entity(&pool, space, "alex").await;

    let evidence = observation(&pool, space, ObsSpec { title: "e", ..Default::default() }).await;
    let old = fact(
        &pool,
        space,
        subject,
        FactSpec { status: "active", value_text: "vim", confidence: 0.8, ..Default::default() },
    )
    .await;
    let new = fact(
        &pool,
        space,
        subject,
        FactSpec {
            value_text: "emacs",
            conflict: true,
            supersedes_id: Some(old),
            evidence: vec![evidence],
            ..Default::default()
        },
    )
    .await;
    let lone =
        fact(&pool, space, subject, FactSpec { predicate: "timezone", value_text: "UTC", ..Default::default() }).await;
    let faded = observation(
        &pool,
        space,
        ObsSpec {
            obs_type: "decision",
            importance: 0.01,
            created_at: Utc::now() - Duration::days(90),
            ..Default::default()
        },
    )
    .await;
    let keep = observation(&pool, space, ObsSpec { obs_type: "decision", importance: 0.9, ..Default::default() }).await;
    let before = [fact_state(&pool, old).await, fact_state(&pool, new).await, fact_state(&pool, lone).await];

    let out = run_planner(&c.state, Some(space)).await.unwrap();
    assert_eq!((out.actions["auto_activate"], out.actions["obs_archive"]), (1, 1));
    let packet: PacketRecord =
        sqlx::query_as("SELECT * FROM work_packets WHERE kind = 'consolidate_facts'").fetch_one(&pool).await.unwrap();
    let counts = c
        .apply(
            &packet,
            json!({"decisions": [{"action": "supersede", "target_id": old, "by_candidate_id": new, "reason": "newer"}]}),
        )
        .await;
    assert_eq!(counts["supersede"], 1);
    assert_eq!(status_of(&pool, old).await, "superseded");
    assert_eq!(status_of(&pool, lone).await, "active");

    let audit_rows = count(&pool, &format!("SELECT count(*) FROM memory_audit WHERE run_id = '{}'", out.run_id)).await;
    assert_eq!(audit_rows, 4, "auto-activate, archive, supersede, create");

    let r = revert(&c, out.run_id).await.unwrap();
    assert_eq!((r.run_id, r.reverted, r.packets_cancelled), (out.run_id, 4, 1));

    let after = [fact_state(&pool, old).await, fact_state(&pool, new).await, fact_state(&pool, lone).await];
    assert_eq!(before, after, "facts are back as they were");
    let archived: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT archived_at FROM observations WHERE id = $1")
            .bind(faded)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(archived.is_none());
    let packet_status: String = text(&pool, "SELECT status FROM work_packets WHERE id = $1", packet.id).await;
    assert_eq!(packet_status, "cancelled");
    let _ = keep;

    // one revert row per change, pointing at the row it undid, in reverse order
    let reverts: Vec<(String, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT reason, reverts_audit_id, actor FROM memory_audit WHERE run_id = $1 AND action = 'revert' ORDER BY seq",
    )
    .bind(out.run_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(reverts.len(), 4);
    assert!(reverts.iter().all(|r| r.1.is_some() && r.2 == "test"));
    assert_eq!(reverts[0].0, "revert of fact.create", "the last change is undone first");
    let revert_before: Value =
        sqlx::query_scalar("SELECT before FROM memory_audit WHERE action = 'revert' AND target_id = $1")
            .bind(old)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(revert_before["status"], "superseded");

    // reverting again undoes nothing again; an unknown run is a 404
    let again = revert(&c, out.run_id).await.unwrap();
    assert_eq!((again.reverted, again.packets_cancelled), (0, 0));
    assert_eq!(revert(&c, new_id()).await.unwrap_err().status.as_u16(), 404);

    // the planner can plan the cluster again afterwards (the cancelled packet is not open)
    let replanned = run_planner(&c.state, Some(space)).await.unwrap();
    assert_eq!(replanned.packets_created["consolidate_facts"], 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn merge_retract_and_promote_revert_exactly(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let space = alice.space_id;
    let subject = entity(&pool, space, "alex").await;
    let e = observation(&pool, space, ObsSpec::default()).await;
    let target = fact(
        &pool,
        space,
        subject,
        FactSpec { status: "active", confidence: 0.6, evidence: vec![e], ..Default::default() },
    )
    .await;
    let cand = fact(&pool, space, subject, FactSpec { value_text: "Vim", confidence: 0.7, ..Default::default() }).await;
    let wrong =
        fact(&pool, space, subject, FactSpec { status: "active", predicate: "wrong", ..Default::default() }).await;
    let travel =
        fact(&pool, space, subject, FactSpec { status: "active", predicate: "travel", ..Default::default() }).await;
    embed_fact(&c, travel, "travel").await;
    let ids = [target, cand, wrong, travel];
    let mut before = Vec::new();
    for id in ids {
        before.push(fact_state(&pool, id).await);
    }

    let run = new_id();
    let packet = c.packet(space, PacketKind::ConsolidateFacts, run_inputs(run, &ids)).await;
    c.apply(
        &packet,
        json!({"decisions": [
            {"action": "merge_into", "target_id": target, "candidate_id": cand, "reason": "same"},
            {"action": "retract", "target_id": wrong, "reason": format!("contradicted by {e}")},
            {"action": "promote_scope", "target_id": travel, "to": "global", "reason": "everywhere"},
            {"action": "set_confidence", "target_id": target, "confidence": 0.99, "reason": "sure"},
        ]}),
    )
    .await;
    assert_eq!(status_of(&pool, wrong).await, "retracted");
    let r = revert(&c, run).await.unwrap();
    assert_eq!(r.reverted, 5, "{r:?}");
    for (id, want) in ids.iter().zip(&before) {
        assert_eq!(&fact_state(&pool, *id).await, want);
    }
    let (kind, key): (String, String) =
        sqlx::query_as("SELECT scope_kind::text, scope_key FROM embeddings WHERE owner_id = $1")
            .bind(travel)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((kind.as_str(), key.as_str()), ("project", "acme"), "the embedding's scope copy follows");
}

#[sqlx::test(migrations = "../../migrations")]
async fn created_summaries_and_procedures_are_removed_and_revisions_undone(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let space = alice.space_id;
    let run = new_id();

    let summary = c
        .packet(space, PacketKind::ConsolidateSummary, json!({"run_id": run, "grain": "day", "scope_kind": "project", "scope_key": "acme", "period": {"start": "2026-10-20", "end": "2026-10-20"}}))
        .await;
    c.apply(&summary, json!({"next_steps": "n", "period": {"start": "2026-10-20", "end": "2026-10-20"}})).await;
    let sum_id: Uuid = sqlx::query_scalar("SELECT id FROM summaries").fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO embeddings (space_id, owner_table, owner_id, part, model, embedding, scope_kind, scope_key) VALUES ($1, 'summaries', $2, 'request', 'bge-m3', $3, 'project', 'acme')")
        .bind(space)
        .bind(sum_id)
        .bind(pgvector::Vector::from(c.tei.vector("x")))
        .execute(&pool)
        .await
        .unwrap();

    let v1 = new_id();
    sqlx::query("INSERT INTO procedures (id, space_id, scope_kind, scope_key, name, task_class, trigger_text, body_md) VALUES ($1, $2, 'project', 'acme', 'Deploy', 'deploy', 't', 'b')")
        .bind(v1)
        .bind(space)
        .execute(&pool)
        .await
        .unwrap();
    let hint = new_id();
    sqlx::query("INSERT INTO procedure_hints (id, space_id, scope_kind, scope_key, task_class, note) VALUES ($1, $2, 'project', 'acme', 'deploy', 'n')")
        .bind(hint)
        .bind(space)
        .execute(&pool)
        .await
        .unwrap();
    let proc_packet = c
        .packet(space, PacketKind::ConsolidateProcedures, json!({"run_id": run, "scope_kind": "project", "scope_key": "acme", "task_class": "deploy", "existing_procedure": {"id": v1}, "hints": [{"id": hint}]}))
        .await;
    c.apply(
        &proc_packet,
        json!({"name": "Deploy", "task_class": "deploy", "trigger_text": "t", "body_md": "b2", "supersedes_id": v1}),
    )
    .await;
    assert_eq!(count(&pool, "SELECT count(*) FROM procedures WHERE status = 'active'").await, 1);
    assert_eq!(count(&pool, "SELECT count(*) FROM procedure_hints WHERE consumed_at IS NOT NULL").await, 1);

    let r = revert(&c, run).await.unwrap();
    assert_eq!((r.reverted, r.packets_cancelled), (3, 2), "summary create, procedure retire, procedure revise: {r:?}");
    assert_eq!(count(&pool, "SELECT count(*) FROM summaries").await, 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM embeddings WHERE owner_table = 'summaries'").await, 0);
    let procs: Vec<(Uuid, String, i32)> =
        sqlx::query_as("SELECT id, status, version FROM procedures").fetch_all(&pool).await.unwrap();
    assert_eq!(procs, vec![(v1, "active".to_string(), 1)]);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM procedure_hints WHERE consumed_at IS NOT NULL").await,
        0,
        "hints can be mined again"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn other_runs_and_later_deletions_are_left_alone(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let space = alice.space_id;
    let subject = entity(&pool, space, "alex").await;
    let a = fact(&pool, space, subject, FactSpec { predicate: "a", ..Default::default() }).await;
    let b = fact(&pool, space, subject, FactSpec { predicate: "b", ..Default::default() }).await;
    let (run1, run2) = (new_id(), new_id());
    for (run, id) in [(run1, a), (run2, b)] {
        let p = c.packet(space, PacketKind::ConsolidateFacts, run_inputs(run, &[id])).await;
        c.apply(&p, json!({"decisions": [{"action": "keep_both", "ids": [id, id], "reason": "activate"}]})).await;
    }
    assert_eq!(status_of(&pool, a).await, "active");
    assert_eq!(status_of(&pool, b).await, "active");
    // run 1's row is hard-deleted later; reverting it must not fail or resurrect it
    sqlx::query("DELETE FROM facts WHERE id = $1").bind(a).execute(&pool).await.unwrap();
    let r = revert(&c, run1).await.unwrap();
    assert_eq!(r.reverted, 0);
    assert_eq!(status_of(&pool, b).await, "active", "run 2 is untouched");
    let r = revert(&c, run2).await.unwrap();
    assert_eq!(r.reverted, 1);
    assert_eq!(status_of(&pool, b).await, "candidate");
}

#[sqlx::test(migrations = "../../migrations")]
async fn reverting_a_remap_keeps_later_changes_to_the_rows(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let space = alice.space_id;
    let sess = session(&pool, space, alice.client_id, "old-key").await;
    let mut tx = c.tx().await;
    let req =
        memvault_consolidate::ops::RemapRequest { from: "old-key".into(), to: "new-key".into(), space_id: Some(space) };
    let run = memvault_consolidate::ops::remap_project(&mut tx, &req, "test").await.unwrap().run_id;
    tx.commit().await.unwrap();
    // the session ends after the remap
    sqlx::query("UPDATE sessions SET ended_at = now(), end_reason = 'exit' WHERE id = $1")
        .bind(sess)
        .execute(&pool)
        .await
        .unwrap();

    let r = revert(&c, run).await.unwrap();
    assert_eq!(r.reverted, 1);
    let (key, ended, reason): (String, bool, Option<String>) =
        sqlx::query_as("SELECT project_key, ended_at IS NOT NULL, end_reason FROM sessions WHERE id = $1")
            .bind(sess)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((key.as_str(), ended, reason.as_deref()), ("old-key", true, Some("exit")));
}

#[sqlx::test(migrations = "../../migrations")]
async fn restoring_a_fact_to_active_over_a_later_holder_is_a_409(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let space = alice.space_id;
    let subject = entity(&pool, space, "alex").await;
    let x = fact(&pool, space, subject, FactSpec { status: "active", value_text: "vim", ..Default::default() }).await;
    let y = fact(&pool, space, subject, FactSpec { value_text: "emacs", supersedes_id: Some(x), ..Default::default() })
        .await;
    let z = fact(&pool, space, subject, FactSpec { value_text: "helix", supersedes_id: Some(y), ..Default::default() })
        .await;
    let (run_a, run_b) = (new_id(), new_id());
    let pa = c.packet(space, PacketKind::ConsolidateFacts, run_inputs(run_a, &[x, y])).await;
    c.apply(
        &pa,
        json!({"decisions": [{"action": "supersede", "target_id": x, "by_candidate_id": y, "reason": "newer"}]}),
    )
    .await;
    let pb = c.packet(space, PacketKind::ConsolidateFacts, run_inputs(run_b, &[y, z])).await;
    c.apply(
        &pb,
        json!({"decisions": [{"action": "supersede", "target_id": y, "by_candidate_id": z, "reason": "newer"}]}),
    )
    .await;
    assert_eq!(status_of(&pool, z).await, "active");

    let err = revert(&c, run_a).await.unwrap_err();
    assert_eq!(err.status.as_u16(), 409, "{err:?}");
    assert!(err.message.contains(&z.to_string()), "{err:?}");
    // rolled back: nothing half-reverted, and reverting in order works
    assert_eq!(status_of(&pool, x).await, "superseded");
    revert(&c, run_b).await.unwrap();
    revert(&c, run_a).await.unwrap();
    assert_eq!(status_of(&pool, x).await, "active");
}
