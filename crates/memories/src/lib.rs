//! memvault-memories: explicit memory writes, work state and the MCP server (spec 6.4, 6.5, 7).
//!
//! - [`memories`]: `POST /v1/memories` (remember) and `DELETE /v1/memories/{id}` (forget).
//! - [`work_state`]: `GET`/`POST /v1/work-state`.
//! - [`mcp`]: the streamable-HTTP MCP service the server mounts at `/mcp`; its tools call the
//!   functions of memvault-recall and of this crate.

pub mod mcp;
pub mod memories;
pub mod work_state;

use axum::Router;
use axum::routing::{delete, get, post};
use memvault_core::AppState;

/// Routes under `/v1` (paths without the prefix): memories (remember, forget), work-state.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/memories", post(memories::create))
        .route("/memories/{id}", delete(memories::delete))
        .route("/work-state", get(work_state::get).post(work_state::post))
}
