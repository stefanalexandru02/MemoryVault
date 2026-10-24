//! The worker sweep: idle windows (9.1), leases expired on the last attempt, owed summaries.

use memvault_core::auth::scopes::TokenKind;
use memvault_core::test_support::seed_identity;
use memvault_ingest::windows::close_idle_windows;
use serde_json::json;
use sqlx::PgPool;

use crate::common::{ctx, ev, prompt, tool};

#[sqlx::test(migrations = "../../migrations")]
async fn idle_windows_close_and_unpinned_prompts_pin_at_lease(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;
    let quiet = c.start_session(&alice, "quiet").await;
    c.events_ok(&alice.token, s, vec![prompt(s, 0, "look around"), tool(s, 1), tool(s, 2)]).await;
    c.events_ok(
        &alice.token,
        quiet,
        vec![prompt(quiet, 0, "hi"), ev(quiet, 1, "system", json!({"source": "startup"}))],
    )
    .await;

    assert_eq!(close_idle_windows(&pool, &c.state.settings).await.unwrap(), 0, "not idle yet");
    sqlx::query("UPDATE session_windows SET last_event_at = now() - interval '91 seconds'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(close_idle_windows(&pool, &c.state.settings).await.unwrap(), 1, "prompt-only windows stay open");
    assert_eq!(close_idle_windows(&pool, &c.state.settings).await.unwrap(), 0);
    assert!(c.packets(quiet).await.is_empty());

    let reason: Option<String> =
        sqlx::query_scalar("SELECT close_reason FROM session_windows WHERE session_id = $1 AND window_no = 1")
            .bind(s)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("idle"));
    let prompt_ref: String = sqlx::query_scalar("SELECT prompt_ref FROM work_packets WHERE session_id = $1")
        .bind(s)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(prompt_ref, "observer.code", "the sweep has no prompt registry");
    let leased = c.lease(&alice.token, json!({})).await;
    assert_eq!(leased[0]["prompt_ref"], "observer.code@v1");
    let pinned: String = sqlx::query_scalar("SELECT prompt_ref FROM work_packets WHERE session_id = $1")
        .bind(s)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pinned, "observer.code@v1");

    // the next event opens window 2
    let out = c.events_ok(&alice.token, s, vec![tool(s, 3)]).await;
    assert_eq!(out["window_no"], 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn last_attempt_expiry_fails_the_packet_and_releases_the_session_summary(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;
    c.events_ok(&alice.token, s, (1..=8).map(|i| tool(s, i)).collect()).await;
    c.lease(&alice.token, json!({})).await;
    c.post(&format!("/v1/sessions/{s}/end"), &alice.token, json!({})).await;

    // the runner vanished on its last attempt
    sqlx::query("UPDATE work_packets SET attempts = max_attempts, lease_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    close_idle_windows(&pool, &c.state.settings).await.unwrap();
    let (status, error): (String, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM work_packets WHERE session_id = $1")
            .bind(s)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(error.as_deref(), Some("lease expired on the last attempt"));
    // nothing to fold (no prompt summaries): the owed session summary is dropped, not created
    let pending: bool = sqlx::query_scalar("SELECT session_summary_pending FROM sessions WHERE id = $1")
        .bind(s)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!pending);
    assert_eq!(c.packets(s).await.len(), 1);
}
