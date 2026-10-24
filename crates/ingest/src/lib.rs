//! memvault-ingest: sessions, events, windows, work packets, extract/summary appliers
//! (spec 6.1, 6.2, 6.6, 9, 10). Skeleton: the owner fills it in, keeping the public signatures.

use axum::Router;
use memvault_core::{AppState, AppliersRegistry};

/// Routes under `/v1` (paths without the prefix): sessions, events, work, prompts.
pub fn router() -> Router<AppState> {
    Router::new()
}

/// Register the `extract`, `summarize_prompt` and `summarize_session` appliers.
pub fn register_appliers(_registry: &mut AppliersRegistry) {}

pub mod windows {
    use memvault_core::Settings;
    use sqlx::PgPool;

    /// Close every window idle for `settings.window_idle_seconds` (spec 9.1) and create its
    /// packets. Called by the worker sweep on the system pool. Returns the windows closed.
    pub async fn close_idle_windows(_pool: &PgPool, _settings: &Settings) -> anyhow::Result<u64> {
        todo!("memvault-ingest: close_idle_windows")
    }
}
