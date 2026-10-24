//! memvault-consolidate: planner, consolidation appliers, revert, admin routes and CLI (spec 6.7,
//! 12). Skeleton: the owner fills it in, keeping the public signatures.

use axum::Router;
use memvault_core::{AppState, AppliersRegistry};

/// Routes under `/v1` (paths without the prefix): `/admin/...`.
pub fn router() -> Router<AppState> {
    Router::new()
}

/// Register the `consolidate_facts`, `consolidate_summary` and `consolidate_procedures` appliers.
pub fn register_appliers(_registry: &mut AppliersRegistry) {}

pub mod planner {
    use memvault_core::schemas::admin::ConsolidateResponse;
    use memvault_core::{ApiError, AppState};
    use uuid::Uuid;

    /// One planning pass (spec 12.1) for one space, or every space when `None`.
    /// `memvault planner` and `POST /v1/admin/consolidate` call it.
    pub async fn run_planner(_state: &AppState, _space_id: Option<Uuid>) -> Result<ConsolidateResponse, ApiError> {
        todo!("memvault-consolidate: run_planner")
    }
}

pub mod admin_cli {
    use memvault_core::AppState;

    /// `memvault admin <args...>` (users, spaces, memberships, tokens, consolidate, revert,
    /// reindex, ...). `args` excludes `admin` itself; parse them with clap inside.
    pub async fn run(_state: AppState, _args: Vec<String>) -> anyhow::Result<()> {
        anyhow::bail!("memvault admin is not implemented yet")
    }
}
