//! RLS through the API: another user's sessions and packets are invisible; a reader of a shared
//! space can neither write sessions into it nor run its packets.

use axum::http::StatusCode;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::schemas::common::MembershipRole;
use memvault_core::test_support::{seed_identity, seed_shared_space};
use serde_json::json;
use sqlx::PgPool;

use crate::common::{PROJECT, ctx, observer_output, tool};

#[sqlx::test(migrations = "../../migrations")]
async fn another_user_sees_nothing(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let bob = seed_identity(&pool, TokenKind::Admin).await;
    let s = c.start_session(&alice, "cc").await;
    c.events_ok(&alice.token, s, (1..=8).map(|i| tool(s, i)).collect()).await;
    c.age_packets().await;

    let resp = c.events(&bob.token, s, vec![tool(s, 9)]).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    let resp = c.post(&format!("/v1/sessions/{s}/end"), &bob.token, json!({})).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    // same external id from Bob's client is a separate session in Bob's space
    let bobs = c.start_session(&bob, "cc").await;
    assert_ne!(bobs, s);

    assert!(c.lease(&bob.token, json!({"max": 10})).await.is_empty());
    assert!(c.lease(&bob.token, json!({"session_id": s})).await.is_empty());

    let id = c.lease(&alice.token, json!({})).await[0]["id"].as_str().unwrap().to_string();
    for path in ["heartbeat", "fail"] {
        let resp = c.post(&format!("/v1/work/{id}/{path}"), &bob.token, json!({"error": "other"})).await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "{path}: {}", resp.text());
    }
    let resp = c.result(&bob.token, &id, observer_output("Injected", &[1])).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    let resp = c.result(&alice.token, &id, observer_output("Real", &[1])).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    // the rows exist, just not for Bob
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM observations WHERE space_id = $1")
        .bind(alice.space_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn shared_space_readers_cannot_write_or_lease(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let reader = seed_identity(&pool, TokenKind::Device).await;
    let writer = seed_identity(&pool, TokenKind::Device).await;
    let team = seed_shared_space(
        &pool,
        "team",
        alice.user_id,
        &[(reader.user_id, MembershipRole::Reader), (writer.user_id, MembershipRole::Writer)],
    )
    .await;

    let body = json!({"external_session_id": "t", "project_key": PROJECT, "space_id": team});
    let resp = c.post("/v1/sessions", &reader.token, body.clone()).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    assert_eq!(resp.error_code().as_deref(), Some("space_forbidden"));

    let resp = c.post("/v1/sessions", &alice.token, body).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(resp.json()["space_id"], team.to_string());
    let s = resp.json()["session_id"].as_str().unwrap().parse().unwrap();
    c.events_ok(&alice.token, s, (1..=8).map(|i| tool(s, i)).collect()).await;
    c.age_packets().await;

    assert!(c.lease(&reader.token, json!({})).await.is_empty(), "readers do not run packets");
    let leased = c.lease(&writer.token, json!({})).await;
    assert_eq!(leased.len(), 1, "writers of the space may run its packets after the preferred window");
    let resp = c.result(&reader.token, leased[0]["id"].as_str().unwrap(), observer_output("x", &[1])).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}
