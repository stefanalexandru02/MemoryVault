//! memvault-recall: candidates, fusion, rerank, budgets, rendering, context cache and the recall
//! routes (spec 6.3, 6.4, 11, 12.5).
//!
//! - [`engine`]: `recall`, `search`, `timeline`, `by_file`, `get_memories`, `explain`, `feedback`
//!   (shared with the MCP tools in memvault-memories; each opens its own `user_tx`).
//! - [`context::render_context`]: the SessionStart block, used by `GET /v1/context` and by the
//!   worker through [`cache::rerender`].
//! - [`cache`]: context cache, `LISTEN memory_invalidate` loop.
//! - [`render`]: deterministic markdown (citations, index with glyph legend, bodies, explain).

pub mod cache;
pub mod candidates;
pub mod context;
mod db;
pub mod engine;
pub mod render;
pub mod scoring;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use memvault_core::auth::{MEMORIES_READ, MEMORIES_WRITE};
use memvault_core::http::{Json, Path, Query};
use memvault_core::schemas::memories::{ByFileQuery, GetRequest, SearchQuery, TimelineQuery};
use memvault_core::schemas::recall::{ContextQuery, FeedbackRequest, RecallRequest};
use memvault_core::{ApiError, AppState, Identity, require_scope};
use uuid::Uuid;

/// Routes under `/v1` (paths without the prefix): recall, context, search, timeline, memories/get,
/// explain, observations/by-file, feedback.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/recall", post(recall))
        .route("/recall/{retrieval_id}/feedback", post(feedback))
        .route("/context", get(context))
        .route("/search", get(search))
        .route("/timeline", get(timeline))
        .route("/memories/get", post(get_memories))
        .route("/memories/{id}/explain", get(explain))
        .route("/observations/by-file", get(by_file))
}

async fn recall(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<RecallRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    Ok(Json(engine::recall(&state, &identity, body).await?))
}

async fn feedback(
    State(state): State<AppState>,
    identity: Identity,
    Path(retrieval_id): Path<Uuid>,
    Json(body): Json<FeedbackRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_WRITE).await?;
    Ok(Json(engine::feedback(&state, &identity, retrieval_id, body).await?))
}

/// `text/markdown` with a strong ETag of the body; `If-None-Match` answers 304.
async fn context(
    State(state): State<AppState>,
    identity: Identity,
    headers: HeaderMap,
    Query(q): Query<ContextQuery>,
) -> Result<Response, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    let body =
        context::render_context(&state, &identity, q.project_key.as_deref(), q.device_id.as_deref(), q.source.as_str())
            .await?;
    let etag = cache::etag(&body);
    let etag_value = HeaderValue::from_str(&etag).map_err(|e| ApiError::internal(e.to_string()))?;
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    if matches {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag_value)]).into_response());
    }
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/markdown; charset=utf-8")),
            (header::ETAG, etag_value),
            (header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache")),
        ],
        body,
    )
        .into_response())
}

async fn search(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<SearchQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    Ok(Json(engine::search(&state, &identity, q).await?))
}

async fn timeline(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<TimelineQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    Ok(Json(engine::timeline(&state, &identity, q).await?))
}

async fn get_memories(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<GetRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    body.validate()?;
    Ok(Json(engine::get_memories(&state, &identity, &body.ids).await?))
}

async fn explain(
    State(state): State<AppState>,
    identity: Identity,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    Ok(Json(engine::explain(&state, &identity, id).await?))
}

async fn by_file(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<ByFileQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    Ok(Json(engine::by_file(&state, &identity, q).await?))
}
