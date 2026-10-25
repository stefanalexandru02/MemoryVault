//! Facts disputed by feedback: retracted by the planner under a run id, undone by a revert, and tagged in profiles.

use chrono::{Duration, Utc};
use memvault_consolidate::planner::run_planner;
use memvault_consolidate::profiles::refresh_scope;
use memvault_consolidate::revert::revert_run;
use memvault_core::ids::new_id;
use memvault_core::schemas::common::{PacketKind, ScopeKind};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::*;

async fn dispute(pool: &PgPool, space: Uuid, user: Uuid, fact: Uuid, verdicts: &[&str]) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for (i, verdict) in verdicts.iter().enumerate() {
        let id = new_id();
        // One row per (user, memory, verdict); the planner only reads them for the reason.
        sqlx::query(
            "INSERT INTO memory_feedback (id, space_id, user_id, target_table, target_id, verdict, note, created_at)
             VALUES ($1, $2, $3, 'facts', $4, $5, 'no longer true', now() + make_interval(secs => $6))",
        )
        .bind(id)
        .bind(space)
        .bind(user)
        .bind(fact)
        .bind(verdict)
        .bind(i as f64)
        .execute(pool)
        .await
        .unwrap();
        ids.push(id);
    }
    sqlx::query("UPDATE facts SET disputed_at = now() WHERE id = $1").bind(fact).execute(pool).await.unwrap();
    ids
}

fn active(predicate: &'static str, value_text: &'static str) -> FactSpec {
    FactSpec { status: "active", predicate, value_text, confidence: 0.8, ..Default::default() }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_disputed_fact_is_retracted_with_the_feedback_as_reason_and_a_revert_restores_it(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let space = alice.space_id;
    let subject = entity(&pool, space, "alex").await;
    let doomed = fact(&pool, space, subject, active("prefers.editor", "vim")).await;
    let keep = fact(&pool, space, subject, active("prefers.shell", "fish")).await;
    let given = dispute(&pool, space, alice.user_id, doomed, &["wrong", "outdated"]).await;

    let out = run_planner(&c.state, Some(space)).await.unwrap();
    assert_eq!(out.actions["feedback_retract"], 1, "{out:?}");
    assert_eq!((status_of(&pool, doomed).await, status_of(&pool, keep).await), ("retracted".into(), "active".into()));
    let (queued, valid_to): (bool, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as("SELECT disputed_at IS NOT NULL, valid_to FROM facts WHERE id = $1")
            .bind(doomed)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!queued && valid_to.is_some());

    let (action, actor, reason): (String, String, String) =
        sqlx::query_as("SELECT action, actor, reason FROM memory_audit WHERE run_id = $1 AND target_id = $2")
            .bind(out.run_id)
            .bind(doomed)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((action.as_str(), actor.as_str()), ("fact.retract", "planner"));
    assert!(reason.starts_with("disputed by feedback: 1 wrong, 1 outdated, 0 helpful"), "{reason}");
    assert!(given.iter().all(|id| reason.contains(&id.to_string())), "{reason}");
    let profile = text(&pool, "SELECT body_md FROM profiles WHERE space_id = $1 AND scope_key = 'acme'", space).await;
    assert!(profile.contains("fish") && !profile.contains("vim"), "{profile}");

    // Nothing is queued any more, so a second pass does nothing.
    let again = run_planner(&c.state, Some(space)).await.unwrap();
    assert!(!again.actions.contains_key("feedback_retract"));

    // The revert puts the fact back, and it is not queued again.
    let mut tx = c.tx().await;
    revert_run(&mut tx, out.run_id, "test").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(status_of(&pool, doomed).await, "active");
    let third = run_planner(&c.state, Some(space)).await.unwrap();
    assert!(!third.actions.contains_key("feedback_retract"));
    assert_eq!(status_of(&pool, doomed).await, "active");
}

#[sqlx::test(migrations = "../../migrations")]
async fn pinned_replaced_and_unqueued_facts_are_left_alone(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let space = alice.space_id;
    let subject = entity(&pool, space, "alex").await;
    let pinned = fact(&pool, space, subject, FactSpec { pinned: true, ..active("prefers.theme", "dark") }).await;
    let quiet = fact(&pool, space, subject, active("prefers.shell", "fish")).await;
    dispute(&pool, space, alice.user_id, pinned, &["wrong"]).await;

    // A candidate waits on the key of the disputed fact: the clusters decide, the feedback step does not.
    let old = fact(&pool, space, subject, active("prefers.editor", "vim")).await;
    let new = fact(
        &pool,
        space,
        subject,
        FactSpec { value_text: "emacs", conflict: true, supersedes_id: Some(old), ..Default::default() },
    )
    .await;
    dispute(&pool, space, alice.user_id, old, &["wrong"]).await;

    let out = run_planner(&c.state, Some(space)).await.unwrap();
    assert!(!out.actions.contains_key("feedback_retract"), "{out:?}");
    for id in [pinned, quiet, old] {
        assert_eq!(status_of(&pool, id).await, "active");
    }
    let packet_facts: serde_json::Value =
        sqlx::query_scalar("SELECT inputs->'fact_ids' FROM work_packets WHERE kind = $1")
            .bind(PacketKind::ConsolidateFacts)
            .fetch_one(&pool)
            .await
            .unwrap();
    let ids: Vec<&str> = packet_facts.as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    assert!(ids.contains(&old.to_string().as_str()) && ids.contains(&new.to_string().as_str()), "{ids:?}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn profiles_tag_stale_and_disputed_facts_and_rerender_when_they_change(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let space = alice.space_id;
    let subject = entity(&pool, space, "alex").await;
    let stale = fact(&pool, space, subject, active("prefers.editor", "vim")).await;
    let fresh = fact(&pool, space, subject, active("prefers.shell", "fish")).await;
    sqlx::query("UPDATE facts SET last_confirmed_at = $2 WHERE id = $1")
        .bind(stale)
        .bind(Utc::now() - Duration::days(130))
        .execute(&pool)
        .await
        .unwrap();
    let line = |body: &str, id: Uuid| {
        body.lines()
            .find(|l| l.contains(&format!("fact#{}", memvault_core::ids::short_id(&id))))
            .unwrap_or_else(|| panic!("no line for {id} in {body}"))
            .to_string()
    };
    let render = || async {
        let mut tx = c.tx().await;
        let changed = refresh_scope(&mut tx, space, ScopeKind::Project, "acme").await.unwrap();
        tx.commit().await.unwrap();
        let body = text(&pool, "SELECT body_md FROM profiles WHERE space_id = $1 AND scope_key = 'acme'", space).await;
        (changed, body)
    };

    let (changed, body) = render().await;
    assert!(changed);
    assert!(line(&body, stale).ends_with("· unconfirmed 4 months]"), "{body}");
    assert!(!line(&body, fresh).contains("unconfirmed") && !line(&body, fresh).contains("disputed"));
    assert!(!render().await.0, "nothing changed, nothing re-rendered");

    sqlx::query("UPDATE facts SET disputed_at = now() WHERE id = $1").bind(fresh).execute(&pool).await.unwrap();
    let (changed, body) = render().await;
    assert!(changed, "the dispute flag is part of what a profile line is rendered from");
    assert!(line(&body, fresh).ends_with("· disputed]"), "{body}");
}
