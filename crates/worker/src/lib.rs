//! memvault-worker: embedding parts and jobs, worker loop, retention (spec 4.8, 13, 14).
//! Skeleton: the owner fills it in, keeping the public signatures.

use axum::Router;
use memvault_core::AppState;
use tokio_util::sync::CancellationToken;

/// Routes under `/v1` (paths without the prefix), if any.
pub fn router() -> Router<AppState> {
    Router::new()
}

/// The `memvault worker` loop (embed jobs, lease expiry and idle-window sweeps, context
/// re-render, retention). Returns when `shutdown` is cancelled.
pub async fn run_worker(_state: AppState, _shutdown: CancellationToken) -> anyhow::Result<()> {
    anyhow::bail!("memvault worker is not implemented yet")
}

pub mod embed {
    /// `jobs(type='embed', payload={"owner_table", "owner_id"})` in the caller's transaction.
    /// Lives in core so memvault-ingest (which this crate depends on) can call it too.
    pub use memvault_core::jobs::enqueue_embed;
}
