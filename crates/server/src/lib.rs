//! Router assembly and process wiring for the `memvault` binary.

use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use memvault_core::db::caps;
use memvault_core::http::{BODY_LIMIT_BYTES, api_router};
use memvault_core::schemas::common::{HealthResponse, HealthTei};
use memvault_core::{AppState, AppliersRegistry};
use tokio_util::sync::CancellationToken;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// Per-request timeout (spec 2: tower-http timeouts).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Every packet applier: ingest (extract, summarize_*) and consolidation (consolidate_*).
pub fn all_appliers() -> AppliersRegistry {
    let mut registry = AppliersRegistry::default();
    memvault_ingest::register_appliers(&mut registry);
    memvault_consolidate::register_appliers(&mut registry);
    registry
}

/// Every module's routes merged under `/v1`, plus `/mcp`, `/health`, `/metrics`, the 8 MB body
/// limit, request ids, tracing and timeouts.
pub fn build_router(state: AppState) -> Router {
    let v1 = Router::new()
        .merge(memvault_ingest::router())
        .merge(memvault_recall::router())
        .merge(memvault_memories::router())
        .merge(memvault_consolidate::router());
    let mcp = memvault_memories::mcp::service(state.clone()).layer(RequestBodyLimitLayer::new(BODY_LIMIT_BYTES));
    api_router(v1)
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .nest_service("/mcp", mcp)
        .with_state(state)
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, REQUEST_TIMEOUT))
        .layer(TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<_>| {
            let request_id = req.headers().get("x-request-id").and_then(|v| v.to_str().ok()).unwrap_or("");
            tracing::info_span!("http", method = %req.method(), path = %req.uri().path(), request_id)
        }))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

/// `GET /health`: `SELECT 1` (2 s) and TEI reachability. `ok`; `degraded` when TEI is down
/// (still 200, spec 14 readiness); `down` with 503 when the database is.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let db_ok = matches!(
        tokio::time::timeout(Duration::from_secs(2), sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&state.db.app))
            .await,
        Ok(Ok(1))
    );
    let pg_search = if db_ok { caps(&state.db.app).await.ok().map(|c| c.pg_search) } else { None };
    let tei = state.tei.health().await;
    let status = match (db_ok, tei.embed && tei.rerank) {
        (false, _) => "down",
        (true, true) => "ok",
        (true, false) => "degraded",
    };
    let body = HealthResponse {
        status: status.into(),
        db: db_ok,
        tei: HealthTei { embed: tei.embed, rerank: tei.rerank },
        version: env!("CARGO_PKG_VERSION").into(),
        pg_search,
    };
    let code = if db_ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, axum::Json(body))
}

/// `GET /metrics`: Prometheus text format.
async fn metrics() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], memvault_core::metrics::prometheus_handle().render())
}

/// `memvault serve` on a bound listener until `shutdown` resolves: the full router, the
/// context-cache invalidation listener (spec 11.6) and Prometheus recorder upkeep.
pub async fn serve(
    state: AppState,
    tcp: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let handle = memvault_core::metrics::prometheus_handle();
    let stop = CancellationToken::new();
    let upkeep = {
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = tick.tick() => handle.run_upkeep(),
                    () = stop.cancelled() => return,
                }
            }
        })
    };
    let listener = memvault_recall::cache::spawn_listener(state.clone(), stop.clone());
    let served = axum::serve(tcp, build_router(state)).with_graceful_shutdown(shutdown).await;
    stop.cancel();
    let _ = tokio::join!(upkeep, listener);
    Ok(served?)
}

/// Resolves on Ctrl-C or SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}
