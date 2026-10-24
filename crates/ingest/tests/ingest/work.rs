//! Spec 6.6 / 9.3: lease order, the preferred-runner window, contention, expiry, heartbeat, fail.

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use memvault_core::auth::scopes::TokenKind;
use memvault_core::test_support::seed_identity;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{Ctx, ctx, second_client, tool};

/// A session of `token`'s client with one closed window (an extract packet); returns the session.
async fn session_with_packet(c: &Ctx, who: &memvault_core::test_support::Seeded, ext: &str) -> Uuid {
    let s = c.start_session(who, ext).await;
    let out = c.events_ok(&who.token, s, (1..=8).map(|i| tool(s, i)).collect()).await;
    assert_eq!(out["window_closed"], true);
    s
}

#[sqlx::test(migrations = "../../migrations")]
async fn preferred_client_first_then_anyone_after_ten_minutes(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (_, runner) = second_client(&pool, &alice, "nightly", TokenKind::Runner).await;
    let s = session_with_packet(&c, &alice, "cc-1").await;

    assert!(c.lease(&runner, json!({})).await.is_empty(), "another client waits 10 minutes");

    let leased = c.lease(&alice.token, json!({"session_id": s})).await;
    assert_eq!(leased.len(), 1);
    let p = &leased[0];
    assert_eq!(p["kind"], "extract");
    assert_eq!(p["prompt_ref"], "observer.code@v1");
    assert_eq!(p["output_schema"], "observer.v1");
    assert_eq!(p["attempts"], 1);
    assert_eq!(p["prompt"]["system"], "You observe.");
    assert!(p["prompt"]["user"].as_str().unwrap().starts_with("window 1: [{"), "{}", p["prompt"]["user"]);
    assert!(p["prompt"]["user_template"].as_str().unwrap().contains("{{ events | tojson }}"));
    let lease_until: DateTime<Utc> = serde_json::from_value(p["lease_until"].clone()).unwrap();
    assert!((lease_until - Utc::now()).num_seconds() > 590);

    // a second packet, older than the preferred-runner window: the runner may take it
    let s2 = session_with_packet(&c, &alice, "cc-2").await;
    c.age_packets().await;
    let leased = c.lease(&runner, json!({"max": 5})).await;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0]["inputs"]["window_no"], 1);
    assert_eq!(c.packets(s2).await[0].2, "leased");

    // kinds and session filters
    let s3 = session_with_packet(&c, &alice, "cc-3").await;
    assert!(c.lease(&alice.token, json!({"kinds": ["summarize_session"]})).await.is_empty());
    assert!(c.lease(&alice.token, json!({"session_id": s})).await.is_empty());
    assert_eq!(c.lease(&alice.token, json!({"session_id": s3})).await.len(), 1);

    let resp = c.post("/v1/work/lease", &alice.token, json!({"max": 0})).await;
    assert_eq!(resp.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../../migrations")]
async fn preferred_packets_come_before_unassigned_ones(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (_, other) = second_client(&pool, &alice, "desktop", TokenKind::Device).await;
    let mine = session_with_packet(&c, &alice, "a").await;
    // the other device's packet is older, but the caller's own comes first
    let theirs = c.start_session_as(&other, "b").await;
    c.events_ok(&other, theirs, (1..=8).map(|i| tool(theirs, i)).collect()).await;
    sqlx::query("UPDATE work_packets SET available_after = now() - interval '1 hour' WHERE session_id = $1")
        .bind(theirs)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE work_packets SET available_after = now() - interval '30 minutes' WHERE session_id = $1")
        .bind(mine)
        .execute(&pool)
        .await
        .unwrap();
    let leased = c.lease(&alice.token, json!({"max": 1})).await;
    assert_eq!(leased[0]["inputs"]["session"]["device"], alice.device_id);
    let leased = c.lease(&alice.token, json!({"max": 1})).await;
    assert_eq!(leased[0]["inputs"]["session"]["device"], "desktop");
}

#[sqlx::test(migrations = "../../migrations")]
async fn two_clients_contending_never_share_a_packet(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (_, r1) = second_client(&pool, &alice, "runner-1", TokenKind::Runner).await;
    let (_, r2) = second_client(&pool, &alice, "runner-2", TokenKind::Runner).await;
    for i in 0..4 {
        session_with_packet(&c, &alice, &format!("cc-{i}")).await;
    }
    c.age_packets().await;

    for _ in 0..2 {
        let (a, b) = tokio::join!(c.lease(&r1, json!({"max": 1})), c.lease(&r2, json!({"max": 1})));
        assert_eq!((a.len(), b.len()), (1, 1));
        assert_ne!(a[0]["id"], b[0]["id"]);
    }
    let (a, b) = tokio::join!(c.lease(&r1, json!({"max": 5})), c.lease(&r2, json!({"max": 5})));
    assert!(a.is_empty() && b.is_empty(), "all four are leased");
    let leased: i64 = sqlx::query_scalar("SELECT count(DISTINCT id) FROM work_packets WHERE status = 'leased'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(leased, 4);
}

#[sqlx::test(migrations = "../../migrations")]
async fn expired_leases_are_handed_out_again(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (_, runner) = second_client(&pool, &alice, "nightly", TokenKind::Runner).await;
    let s = session_with_packet(&c, &alice, "cc").await;
    let first = c.lease(&alice.token, json!({"lease_seconds": 30})).await;
    let id = first[0]["id"].as_str().unwrap().to_string();

    assert!(c.lease(&runner, json!({})).await.is_empty(), "lease still valid");
    sqlx::query("UPDATE work_packets SET lease_until = now() - interval '1 second', available_after = now() - interval '1 hour'")
        .execute(&pool)
        .await
        .unwrap();
    let again = c.lease(&runner, json!({})).await;
    assert_eq!(again[0]["id"], id.as_str());
    assert_eq!(again[0]["attempts"], 2);

    // the first holder lost the lease
    let resp = c.result(&alice.token, &id, crate::common::observer_output("Late result", &[1])).await;
    assert_eq!(resp.status, StatusCode::CONFLICT);
    assert_eq!(resp.error_code().as_deref(), Some("lease_conflict"));
    let resp = c.post(&format!("/v1/work/{id}/heartbeat"), &alice.token, json!({})).await;
    assert_eq!(resp.status, StatusCode::CONFLICT);
    let resp = c.result(&runner, &id, crate::common::observer_output("On time", &[1])).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(c.packets(s).await[0].2, "done");
}

#[sqlx::test(migrations = "../../migrations")]
async fn heartbeat_extends_once(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    session_with_packet(&c, &alice, "cc").await;
    let p = c.lease(&alice.token, json!({"lease_seconds": 60})).await;
    let id = p[0]["id"].as_str().unwrap();
    let before: DateTime<Utc> = serde_json::from_value(p[0]["lease_until"].clone()).unwrap();

    let resp = c.post(&format!("/v1/work/{id}/heartbeat"), &alice.token, json!({})).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let after: DateTime<Utc> = serde_json::from_value(resp.json()["lease_until"].clone()).unwrap();
    assert!(after >= before);
    assert!((after - Utc::now()).num_seconds() <= 60);

    let resp = c.post(&format!("/v1/work/{id}/heartbeat"), &alice.token, json!({})).await;
    assert_eq!(resp.status, StatusCode::CONFLICT);
    assert_eq!(resp.error_code().as_deref(), Some("lease_extension_exhausted"));
    let resp = c.post(&format!("/v1/work/{}/heartbeat", Uuid::now_v7()), &alice.token, json!({})).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations")]
async fn fail_backs_off_then_gives_up(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = session_with_packet(&c, &alice, "cc").await;
    let id = c.lease(&alice.token, json!({})).await[0]["id"].as_str().unwrap().to_string();

    let resp =
        c.post(&format!("/v1/work/{id}/fail"), &alice.token, json!({"error": "timeout", "detail": "180 s"})).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let out = resp.json();
    assert_eq!((out["status"].clone(), out["attempts"].clone()), (json!("pending"), json!(1)));
    let after: DateTime<Utc> = serde_json::from_value(out["available_after"].clone()).unwrap();
    assert!((50..=60).contains(&(after - Utc::now()).num_seconds()));
    assert!(c.lease(&alice.token, json!({})).await.is_empty(), "backing off");

    // a quota failure frees the packet for any runner and doubles the backoff
    sqlx::query("UPDATE work_packets SET available_after = now()").execute(&pool).await.unwrap();
    c.lease(&alice.token, json!({})).await;
    let out = c.post(&format!("/v1/work/{id}/fail"), &alice.token, json!({"error": "quota_exhausted"})).await.json();
    let after: DateTime<Utc> = serde_json::from_value(out["available_after"].clone()).unwrap();
    assert!((1790..=1800).contains(&(after - Utc::now()).num_seconds()));
    let (preferred, last_error): (Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT preferred_client_id, last_error FROM work_packets WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((preferred, last_error.as_deref()), (None, Some("quota_exhausted")));

    // the third attempt fails for good
    sqlx::query("UPDATE work_packets SET available_after = now()").execute(&pool).await.unwrap();
    assert_eq!(c.lease(&alice.token, json!({})).await[0]["attempts"], 3);
    let out = c.post(&format!("/v1/work/{id}/fail"), &alice.token, json!({"error": "other"})).await.json();
    assert_eq!((out["status"].clone(), out["available_after"].clone()), (json!("failed"), json!(null)));
    assert_eq!(c.packets(s).await[0].2, "failed");
    sqlx::query("UPDATE work_packets SET available_after = now() - interval '1 hour'").execute(&pool).await.unwrap();
    assert!(c.lease(&alice.token, json!({})).await.is_empty());

    let resp = c.post(&format!("/v1/work/{id}/fail"), &alice.token, json!({"error": "other"})).await;
    assert_eq!(resp.error_code().as_deref(), Some("not_leased"));
    let resp = c.post(&format!("/v1/work/{id}/fail"), &alice.token, json!({"error": "nope"})).await;
    assert_eq!(resp.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../../migrations")]
async fn unknown_prompt_fails_the_packet_at_lease(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = session_with_packet(&c, &alice, "cc").await;
    sqlx::query("UPDATE work_packets SET prompt_ref = 'observer.gone@v1'").execute(&pool).await.unwrap();
    assert!(c.lease(&alice.token, json!({})).await.is_empty());
    let (status, error): (String, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM work_packets WHERE session_id = $1")
            .bind(s)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert!(error.unwrap().starts_with("prompt_error"));
}
