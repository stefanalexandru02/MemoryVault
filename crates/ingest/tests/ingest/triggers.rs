//! Triggered consolidation: a session end (once its extraction is done) and a busy scope each queue a
//! scoped `plan` job, one per scope, and only for the scopes the caller's own rows name.

use memvault_core::auth::scopes::TokenKind;
use memvault_core::test_support::{Seeded, seed_identity};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{Ctx, PROJECT, ctx_with, observer_output, prompt, tool};

/// `(trigger, space, scope kind, scope key)` of the live plan jobs, oldest first.
async fn plan_jobs(pool: &PgPool) -> Vec<(String, Uuid, String, String)> {
    let rows: Vec<Value> = sqlx::query_scalar("SELECT payload FROM jobs WHERE type = 'plan' ORDER BY created_at, id")
        .fetch_all(pool)
        .await
        .unwrap();
    rows.iter()
        .map(|p| {
            (
                p["trigger"].as_str().unwrap().to_string(),
                p["space_id"].as_str().unwrap().parse().unwrap(),
                p["scope"]["kind"].as_str().unwrap().to_string(),
                p["scope"]["key"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// A session with one window of tool uses and no Stop, so the extract packet closes on `end`.
async fn session_with_work(c: &Ctx, who: &Seeded, external: &str) -> Uuid {
    let s = c.start_session(who, external).await;
    c.events_ok(&who.token, s, vec![prompt(s, 0, "fix the parser"), tool(s, 1), tool(s, 2)]).await;
    s
}

async fn extract_done(c: &Ctx, who: &Seeded, session: Uuid) {
    let leased = c.lease(&who.token, json!({"session_id": session, "kinds": ["extract"]})).await;
    let id = leased[0]["id"].as_str().unwrap();
    let resp = c.result(&who.token, id, observer_output("Parser drops the last token", &[1])).await;
    assert!(resp.status.is_success(), "{}", resp.text());
}

async fn end(c: &Ctx, who: &Seeded, session: Uuid) {
    let resp = c.post(&format!("/v1/sessions/{session}/end"), &who.token, json!({"reason": "exit"})).await;
    assert!(resp.status.is_success(), "{}", resp.text());
}

#[sqlx::test(migrations = "../../migrations")]
async fn session_end_queues_a_plan_job_once_extraction_is_done(pool: PgPool) {
    let c = ctx_with(pool.clone(), |s| s.consolidate_observation_threshold = 0);
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = session_with_work(&c, &alice, "cc").await;

    // the session has ended but its extract packet is still pending: nothing to plan yet
    end(&c, &alice, s).await;
    assert!(plan_jobs(&pool).await.is_empty());

    extract_done(&c, &alice, s).await;
    assert_eq!(
        plan_jobs(&pool).await,
        [("session_end".to_string(), alice.space_id, "project".to_string(), PROJECT.to_string())]
    );

    // a second end of the same session finds the job waiting and queues nothing
    end(&c, &alice, s).await;
    assert_eq!(plan_jobs(&pool).await.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_running_session_queues_nothing(pool: PgPool) {
    let c = ctx_with(pool.clone(), |s| s.consolidate_observation_threshold = 0);
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = session_with_work(&c, &alice, "cc").await;
    c.events_ok(&alice.token, s, vec![tool(s, 3), tool(s, 4), tool(s, 5), tool(s, 6), tool(s, 7), tool(s, 8)]).await;
    extract_done(&c, &alice, s).await;
    assert!(plan_jobs(&pool).await.is_empty(), "an extract result alone does not end a session");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_session_end_never_plans_another_users_scope(pool: PgPool) {
    let c = ctx_with(pool.clone(), |s| s.consolidate_observation_threshold = 0);
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let bob = seed_identity(&pool, TokenKind::Device).await;
    // both work in the same project key, in their own spaces
    let (sa, sb) = (session_with_work(&c, &alice, "cc").await, session_with_work(&c, &bob, "cc").await);
    end(&c, &alice, sa).await;
    end(&c, &bob, sb).await;

    extract_done(&c, &alice, sa).await;
    let jobs = plan_jobs(&pool).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].1, alice.space_id, "{jobs:?}");

    extract_done(&c, &bob, sb).await;
    let spaces: Vec<Uuid> = plan_jobs(&pool).await.into_iter().map(|j| j.1).collect();
    assert_eq!(spaces, [alice.space_id, bob.space_id], "one job per space even for the same scope key");
}

#[sqlx::test(migrations = "../../migrations")]
async fn enough_new_observations_queue_a_threshold_job(pool: PgPool) {
    let c = ctx_with(pool.clone(), |s| s.consolidate_observation_threshold = 2);
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;
    c.events_ok(&alice.token, s, (0..8).map(|i| tool(s, i)).collect()).await;
    extract_done(&c, &alice, s).await;
    assert!(plan_jobs(&pool).await.is_empty(), "one new observation is below the threshold of two");

    sqlx::query(
        "INSERT INTO observations (id, space_id, scope_kind, scope_key, type, title, extracted_by, content_hash)
         VALUES (gen_random_uuid(), $1, 'project', $2, 'change', 'earlier', '{}', 'h')",
    )
    .bind(alice.space_id)
    .bind(PROJECT)
    .execute(&pool)
    .await
    .unwrap();
    c.events_ok(&alice.token, s, (8..16).map(|i| tool(s, i)).collect()).await;
    let leased = c.lease(&alice.token, json!({"session_id": s, "kinds": ["extract"]})).await;
    let id = leased[0]["id"].as_str().unwrap();
    c.result(&alice.token, id, observer_output("Second finding", &[9])).await;
    assert_eq!(
        plan_jobs(&pool).await,
        [("threshold".to_string(), alice.space_id, "project".to_string(), PROJECT.to_string())]
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn observations_before_the_last_plan_do_not_count(pool: PgPool) {
    let c = ctx_with(pool.clone(), |s| s.consolidate_observation_threshold = 1);
    let alice = seed_identity(&pool, TokenKind::Device).await;
    sqlx::query(
        "INSERT INTO scope_planning (space_id, scope_kind, scope_key, last_planned_at)
         VALUES ($1, 'project', $2, now() + interval '1 hour')",
    )
    .bind(alice.space_id)
    .bind(PROJECT)
    .execute(&pool)
    .await
    .unwrap();
    let s = c.start_session(&alice, "cc").await;
    c.events_ok(&alice.token, s, (0..8).map(|i| tool(s, i)).collect()).await;
    extract_done(&c, &alice, s).await;
    assert!(plan_jobs(&pool).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_threshold_of_zero_turns_the_trigger_off(pool: PgPool) {
    let c = ctx_with(pool.clone(), |s| s.consolidate_observation_threshold = 0);
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;
    c.events_ok(&alice.token, s, (0..8).map(|i| tool(s, i)).collect()).await;
    extract_done(&c, &alice, s).await;
    assert!(plan_jobs(&pool).await.is_empty());
}
