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
    let pg_search: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_search')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(body["pg_search"], pg_search);
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

    // /mcp is mounted and applies bearer auth itself
    let r = send(&app, Method::POST, "/mcp", None, Some(serde_json::json!({}))).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
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

/// Collects what a tracing subscriber writes.
#[derive(Clone, Default)]
struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;

    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn request_logs_carry_user_client_and_space(pool: PgPool) {
    use memvault_core::auth::scopes::TokenKind;
    use memvault_core::test_support::seed_identity;

    let alice = seed_identity(&pool, TokenKind::Device).await;
    let app = build_router(test_state(pool.clone()));
    let sink = Sink::default();
    let subscriber =
        tracing_subscriber::fmt().json().with_max_level(tracing::Level::DEBUG).with_writer(sink.clone()).finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let r = send(&app, Method::GET, "/v1/search?q=anything", Some(&alice.token), None).await;
    drop(guard);
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());

    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let spans: Vec<serde_json::Value> = logs
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|l| l.get("span").cloned())
        .filter(|s| s["name"] == "http")
        .collect();
    let last = spans.last().unwrap_or_else(|| panic!("no http span in\n{logs}"));
    assert_eq!(last["user_id"], alice.user_id.to_string(), "{last}");
    assert_eq!(last["client_id"], alice.client_id.to_string(), "{last}");
    assert_eq!(last["space_id"], alice.space_id.to_string(), "{last}");
    assert!(!logs.contains(&alice.token), "tokens are never logged");
}
