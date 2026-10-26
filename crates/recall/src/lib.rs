//! memvault-recall: candidates, fusion, rerank, budgets, rendering, context cache and the recall
//! routes (spec 6.3, 6.4, 11, 12.5).
//!
//! - [`engine`]: `recall`, `search`, `timeline`, `by_file`, `get_memories`, `explain`, `feedback`
//!   (shared with the MCP tools in memvault-memories; each opens its own `user_tx`).
//! - [`context::render_context`]: the SessionStart block, used by `GET /v1/context` and by the
//!   worker through [`cache::rerender`].
//! - [`smart`]: `POST /v1/context/prompt`, the memories a session's first prompt matches (smart injection).
//! - [`cache`]: context cache, `LISTEN memory_invalidate` loop.
//! - [`settings`]: `GET`/`PUT /v1/me/settings` (the default injection mode); [`stats`]: `GET /v1/stats/recall`.
//! - [`render`]: deterministic markdown (citations, index with glyph legend, bodies, trace).

pub mod cache;
pub mod candidates;
pub mod context;
pub mod engine;
pub mod render;
pub mod scoring;
pub mod settings;
pub mod smart;
pub mod stats;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use memvault_core::auth::{MEMORIES_READ, MEMORIES_WRITE};
use memvault_core::http::{Json, Path, Query};
use memvault_core::schemas::memories::{ByFileQuery, GetRequest, SearchQuery, TimelineQuery};
use memvault_core::schemas::recall::{
    ContextQuery, FeedbackRequest, PromptContextRequest, RecallRequest, StatsQuery, UserSettings,
};
use memvault_core::{ApiError, AppState, Identity, require_scope};
use uuid::Uuid;

/// Routes under `/v1` (paths without the prefix): recall, context, search, timeline, memories/get,
/// explain, history, observations/by-file, feedback, me/settings, stats/recall.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/recall", post(recall))
        .route("/recall/{retrieval_id}/feedback", post(feedback))
        .route("/context", get(context))
        .route("/context/prompt", post(prompt_context))
        .route("/search", get(search))
        .route("/timeline", get(timeline))
        .route("/memories/get", post(get_memories))
        .route("/memories/{id}/explain", get(explain))
        .route("/memories/{id}/history", get(history))
        .route("/observations/by-file", get(by_file))
        .route("/me/settings", get(get_settings).put(put_settings))
        .route("/stats/recall", get(recall_stats))
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

/// The injection mode a context block was rendered in (absent for `resume`, which has none), for clients that
/// act on it: `smart` asks them for the first prompt's memories.
const INJECTION_HEADER: HeaderName = HeaderName::from_static("x-memory-injection");

/// `text/markdown` with a strong ETag of the body; `If-None-Match` answers 304.
async fn context(
    State(state): State<AppState>,
    identity: Identity,
    headers: HeaderMap,
    Query(q): Query<ContextQuery>,
) -> Result<Response, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    q.validate()?;
    let options = context::ContextOptions { mode: q.mode, rows: q.rows, types: q.types.unwrap_or_default() };
    let block = context::render_context_with(
        &state,
        &identity,
        q.project_key.as_deref(),
        q.device_id.as_deref(),
        q.source.as_str(),
        &options,
    )
    .await?;
    let (body, mode) = (block.body, block.mode);
    let etag = cache::etag(&body);
    let etag_value = HeaderValue::from_str(&etag).map_err(|e| ApiError::internal(e.to_string()))?;
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    let mut response = if matches {
        (StatusCode::NOT_MODIFIED, [(header::ETAG, etag_value)]).into_response()
    } else {
        (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("text/markdown; charset=utf-8")),
                (header::ETAG, etag_value),
                (header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache")),
            ],
            body,
        )
            .into_response()
    };
    if let Some(mode) = mode {
        response.headers_mut().insert(INJECTION_HEADER, HeaderValue::from_static(mode.as_str()));
    }
    Ok(response)
}

/// The memories the first prompt of a session matches, as `text/markdown`; an empty body when none is relevant.
async fn prompt_context(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<PromptContextRequest>,
) -> Result<Response, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    body.validate()?;
    let text = smart::prompt_context(&state, &identity, body).await?;
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/markdown; charset=utf-8")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        text,
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

async fn history(
    State(state): State<AppState>,
    identity: Identity,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    Ok(Json(engine::history(&state, &identity, id).await?))
}

async fn by_file(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<ByFileQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    Ok(Json(engine::by_file(&state, &identity, q).await?))
}

async fn get_settings(State(state): State<AppState>, identity: Identity) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    Ok(Json(settings::get(&state, &identity).await?))
}

async fn put_settings(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<UserSettings>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_WRITE).await?;
    Ok(Json(settings::put(&state, &identity, body).await?))
}

async fn recall_stats(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<StatsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    Ok(Json(stats::recall_stats(&state, &identity, &q).await?))
}
