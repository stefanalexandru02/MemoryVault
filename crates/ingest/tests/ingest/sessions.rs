//! Spec 6.1 sessions and 10.1 prompts route.

use axum::http::{Method, StatusCode};
use memvault_core::auth::scopes::TokenKind;
use memvault_core::test_support::{seed_identity, send};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{PROJECT, ctx, prompt, tool};

#[sqlx::test(migrations = "../../migrations")]
async fn create_is_idempotent_per_client(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let body = json!({"external_session_id": "cc-1", "project_key": PROJECT, "metadata": {"model": "opus"}});
    let first = c.post("/v1/sessions", &alice.token, body.clone()).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.text());
    let out = first.json();
    assert_eq!(out["space_id"], alice.space_id.to_string());
    assert_eq!(out["window_policy"], json!({"tool_uses": 8, "idle_seconds": 90}));

    let again = c.post("/v1/sessions", &alice.token, body).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.json()["session_id"], out["session_id"]);

    let (count, project): (i64, Option<String>) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM sessions), (SELECT current_project_key FROM clients WHERE id = $1)",
    )
    .bind(alice.client_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((count, project.as_deref()), (1, Some(PROJECT)));
}

#[sqlx::test(migrations = "../../migrations")]
async fn create_checks_scope_and_space(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let bob = seed_identity(&pool, TokenKind::Device).await;
    let runner = seed_identity(&pool, TokenKind::Runner).await;

    let resp = c.post("/v1/sessions", &runner.token, json!({"external_session_id": "x"})).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    assert_eq!(resp.error_code().as_deref(), Some("insufficient_scope"));

    let resp =
        c.post("/v1/sessions", &bob.token, json!({"external_session_id": "x", "space_id": alice.space_id})).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    assert_eq!(resp.error_code().as_deref(), Some("space_forbidden"));

    let resp = c.post("/v1/sessions", &alice.token, json!({"external_session_id": ""})).await;
    assert_eq!(resp.status, StatusCode::UNPROCESSABLE_ENTITY);

    let resp = send(&c.app, Method::POST, "/v1/sessions", None, Some(json!({"external_session_id": "x"}))).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations")]
async fn end_closes_the_open_window(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc-end").await;
    c.events_ok(&alice.token, s, vec![prompt(s, 0, "fix the parser"), tool(s, 1)]).await;

    let resp = c.post(&format!("/v1/sessions/{s}/end"), &alice.token, json!({"reason": "prompt_input_exit"})).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let out = resp.json();
    assert_eq!(out["window_closed"], true);
    assert_eq!(out["packets_created"], json!(["extract"]));
    let (reason, pending, close): (Option<String>, bool, Option<String>) = sqlx::query_as(
        "SELECT s.end_reason, s.session_summary_pending, w.close_reason
           FROM sessions s JOIN session_windows w ON w.session_id = s.id WHERE s.id = $1",
    )
    .bind(s)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reason.as_deref(), Some("prompt_input_exit"));
    assert!(pending, "summarize_session waits for the extract packet");
    assert_eq!(close.as_deref(), Some("end"));

    // a body is optional; a second end creates nothing new
    let resp = send(&c.app, Method::POST, &format!("/v1/sessions/{s}/end"), Some(&alice.token), None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["window_closed"], false);
    assert_eq!(c.packets(s).await.len(), 1);

    let resp = c.post(&format!("/v1/sessions/{}/end", Uuid::now_v7()), &alice.token, json!({})).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations")]
async fn prompts_route(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let get = |uri: &'static str| send(&c.app, Method::GET, uri, Some(&alice.token), None);

    let resp = get("/v1/prompts/observer.code").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let out = resp.json();
    assert_eq!(out["prompt_ref"], "observer.code@v1");
    assert_eq!(out["output_schema"], "observer.v1");
    assert_eq!(out["versions"], json!(["v1"]));

    let resp = get("/v1/prompts/summary.session@v2").await;
    assert_eq!(resp.json()["model_hint"], "opus");
    let resp = get("/v1/prompts/observer.code?version=v1").await;
    assert_eq!(resp.status, StatusCode::OK);

    for missing in ["/v1/prompts/nope", "/v1/prompts/observer.code@v9"] {
        let resp = get(missing).await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
        assert_eq!(resp.error_code().as_deref(), Some("not_found"));
    }
}
