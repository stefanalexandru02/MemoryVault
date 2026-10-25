//! Lives in its own test binary: a thread-local tracing subscriber races with other tests registering
//! callsites in the same process, which silently drops events and makes the assertions flaky.

use axum::http::{Method, StatusCode};
use memvault_core::test_support::{send, test_state};
use memvault_server::build_router;
use sqlx::PgPool;

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
