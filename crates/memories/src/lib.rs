//! memvault-memories: explicit memory writes, work state and the MCP server (spec 6.4, 6.5, 7).
//!
//! - [`memories`]: `POST /v1/memories` (remember) and `DELETE /v1/memories/{id}` (forget).
//! - [`feedback`]: `POST /v1/memories/{id}/feedback` (helpful, wrong, outdated).
//! - [`sources`] and [`chunk`]: `POST`/`GET`/`DELETE /v1/sources`, documents chunked into searchable memory.
//! - [`work_state`]: `GET`/`POST /v1/work-state`.
//! - `GET /v1/me`: who the bearer token is (used by the web viewer to show the user and hide admin pages).
//! - [`mcp`]: the streamable-HTTP MCP service the server mounts at `/mcp`; its tools call the
//!   functions of memvault-recall and of this crate.

pub mod chunk;
pub mod feedback;
pub mod mcp;
pub mod memories;
pub mod sources;
pub mod work_state;

use axum::Router;
use axum::routing::{delete, get, post};
use memvault_core::http::Json;
use memvault_core::{ApiError, AppState, Identity};
use serde_json::json;

/// Routes under `/v1` (paths without the prefix): memories (remember, forget, feedback), sources, work-state.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/memories", post(memories::create))
        .route("/memories/{id}", delete(memories::delete))
        .route("/memories/{id}/feedback", post(feedback::route))
        .route("/sources", get(sources::list).post(sources::put).delete(sources::remove))
        .route("/work-state", get(work_state::get).post(work_state::post))
        .route("/me", get(me))
}

/// The caller's identity, from the token alone (no scope needed, no database read).
async fn me(identity: Identity) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(json!({
        "user_id": identity.user_id,
        "client_id": identity.client_id,
        "device_id": identity.device_id,
        "client_kind": identity.client_kind,
        "token_kind": identity.token_kind,
        "scopes": identity.scopes,
    })))
}
