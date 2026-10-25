//! HTTP plumbing shared by every router: extractors whose rejections use the spec error shape, and
//! the `/v1` assembly used by the server and by tests.
//!
//! Module crates write handlers with `memvault_core::http::{Json, Query, Path}` instead of the axum
//! originals so that malformed requests answer `{"error": {...}}` like everything else.

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::ApiError;
use crate::state::AppState;

/// Request body limit for the whole API (spec 14: ingress `proxy-body-size: 8m`).
pub const BODY_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// JSON body extractor / response; rejections are [`ApiError`]s (422 `invalid_request`, 413, 415).
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<S, T> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let axum::Json(value) = axum::Json::<T>::from_request(req, state).await?;
        Ok(Json(value))
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// Query-string extractor; rejections are 422 `invalid_request`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<S, T> FromRequestParts<S> for Query<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let axum::extract::Query(value) = axum::extract::Query::<T>::from_request_parts(parts, state).await?;
        Ok(Query(value))
    }
}

/// Path-parameter extractor; rejections use the spec error shape.
#[derive(Debug, Clone, Copy, Default)]
pub struct Path<T>(pub T);

impl<S, T> FromRequestParts<S> for Path<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let axum::extract::Path(value) = axum::extract::Path::<T>::from_request_parts(parts, state).await?;
        Ok(Path(value))
    }
}

/// Nest the merged module routers under `/v1` and add the JSON 404/405 fallbacks and the body
/// limit. Module routers declare their paths without the `/v1` prefix (`/sessions`, not
/// `/v1/sessions`). The server adds `/health`, `/metrics` and `/mcp` on top.
pub fn api_router(v1: Router<AppState>) -> Router<AppState> {
    Router::new()
        .nest("/v1", v1)
        .fallback(|| async { ApiError::not_found("Not Found") })
        .method_not_allowed_fallback(|| async {
            ApiError::new(axum::http::StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "Method Not Allowed")
        })
        .layer(DefaultBodyLimit::max(BODY_LIMIT_BYTES))
}
