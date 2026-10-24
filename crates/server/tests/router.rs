use axum::http::{Method, StatusCode};
use memvault_core::AppliersRegistry;
use memvault_core::test_support::{send, test_settings, test_state, test_state_with};
use memvault_server::build_router;
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
async fn health_metrics_and_fallbacks(pool: PgPool) {
    let app = build_router(test_state(pool.clone()));
    let r = send(&app, Method::GET, "/health", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    let body = r.json();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["db"], true);
    assert_eq!(body["tei"], serde_json::json!({"embed": true, "rerank": true}));
    assert_eq!(body["pg_search"], false);
    assert!(r.headers.contains_key("x-request-id"));

    let r = send(&app, Method::GET, "/metrics", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers["content-type"].to_str().unwrap().starts_with("text/plain"));

    let r = send(&app, Method::GET, "/v1/does-not-exist", None, None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(
        r.json(),
        serde_json::json!({"error": {"code": "not_found", "message": "Not Found", "retryable": false}})
    );

    // /mcp is mounted (the memories stub answers 501 until the MCP server exists)
    let r = send(&app, Method::POST, "/mcp", None, Some(serde_json::json!({}))).await;
    assert_ne!(r.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations")]
async fn health_degraded_when_tei_down(pool: PgPool) {
    let (state, tei) = test_state_with(pool, test_settings(), AppliersRegistry::default());
    tei.set_down(true);
    let r = send(&build_router(state), Method::GET, "/health", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["status"], "degraded");
}

#[tokio::test]
async fn health_down_without_database() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .unwrap();
    let r = send(&build_router(test_state(pool)), Method::GET, "/health", None, None).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.json()["status"], "down");
}
