//! memvault-evals: record, redact, replay, compare (spec 16). Skeleton: the owner fills it in,
//! keeping the public signatures.

use axum::Router;
use memvault_core::AppState;

/// Routes under `/v1` (paths without the prefix), if any.
pub fn router() -> Router<AppState> {
    Router::new()
}

pub mod cli {
    use memvault_core::AppState;

    /// `memvault evals <args...>` (`args` excludes `evals`); parse them with clap inside.
    pub async fn run(_state: AppState, _args: Vec<String>) -> anyhow::Result<()> {
        anyhow::bail!("memvault evals is not implemented yet")
    }
}
