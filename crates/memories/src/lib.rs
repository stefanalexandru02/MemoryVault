//! memvault-memories: memories + work-state routes and the MCP server (spec 6.4, 6.5, 7).
//! Skeleton: the owner fills it in, keeping the public signatures.

use axum::Router;
use memvault_core::AppState;

/// Routes under `/v1` (paths without the prefix): memories (remember, forget), work-state.
pub fn router() -> Router<AppState> {
    Router::new()
}

pub mod mcp {
    use axum::Router;
    use memvault_core::{ApiError, AppState};

    /// The streamable-HTTP MCP service mounted by the server at `/mcp` (stateless, same bearer
    /// auth as REST via `memvault_core::auth::authenticate_header`). Stub: 501 for every request.
    pub fn service(state: AppState) -> Router {
        Router::new().fallback(|| async { ApiError::not_implemented("MCP") }).with_state(state)
    }
}
