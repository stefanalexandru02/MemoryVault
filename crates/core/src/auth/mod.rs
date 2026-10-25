//! Authentication and authorization (spec 5): tokens, scopes, the [`Identity`] extractor, space
//! resolution, the per-token rate limit and tenancy bootstrap helpers.
//!
//! Handler pattern:
//!
//! ```ignore
//! async fn create(State(state): State<AppState>, identity: Identity, Json(body): Json<SessionCreate>)
//!     -> Result<impl IntoResponse, ApiError> {
//!     require_scope(&state, &identity, scopes::EVENTS_WRITE).await?;   // 403 + rate limit
//!     let mut tx = user_tx(&state.db.app, &identity).await?;           // RLS as the caller
//!     let space = resolve_write_space(&mut tx, &identity, body.space_id).await?;
//!     ...
//! }
//! ```

pub mod admin;
pub mod rate_limit;
pub mod scopes;
pub mod spaces;
pub mod tokens;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

pub use rate_limit::check_rate_limit;
pub use scopes::{ADMIN, EVENTS_WRITE, MEMORIES_READ, MEMORIES_WRITE, TokenKind, WORK_LEASE};
pub use spaces::{readable_space_ids, resolve_read_spaces, resolve_write_space, writable_space_ids};
pub use tokens::{MintOptions, MintedToken, mint_token, revoke_token, verify_token};

/// The authenticated caller, resolved from a bearer token. As an axum extractor it authenticates
/// the request (401 on failure) and caches the result in the request extensions; it does not
/// check scopes, see [`require_scope`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub user_id: Uuid,
    pub client_id: Uuid,
    pub scopes: Vec<String>,
    pub default_space_id: Option<Uuid>,
    pub token_id: Uuid,
    /// `device` | `runner` | `admin`.
    pub token_kind: String,
    /// `clients.device_id` of the token's client.
    pub device_id: String,
    /// `clients.kind`.
    pub client_kind: String,
}

impl Identity {
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// 403 `insufficient_scope` unless the token has `scope` (no rate limiting; use it for a
    /// second scope after [`require_scope`], e.g. `admin` for `?hard=true`).
    pub fn ensure(&self, scope: &str) -> Result<(), ApiError> {
        if self.has(scope) {
            Ok(())
        } else {
            Err(ApiError::forbidden("insufficient_scope", format!("token lacks scope {scope}")))
        }
    }

    /// `memory_audit.actor` for writes made by this caller.
    pub fn actor(&self) -> String {
        format!("client:{}", self.client_id)
    }
}

/// Resolve an `Authorization: Bearer mem_...` header value, or a 401.
pub async fn authenticate_header(state: &AppState, authorization: Option<&str>) -> Result<Identity, ApiError> {
    let token = authorization
        .and_then(|h| h.get(..7).filter(|p| p.eq_ignore_ascii_case("bearer ")).map(|_| h[7..].trim()))
        .ok_or_else(|| ApiError::unauthorized("unauthorized", "missing bearer token"))?;
    let mut conn = state.db.app.acquire().await?;
    let identity = verify_token(&mut conn, token, &state.settings.token_pepper).await?;
    // Log lines of this request carry who made it (the server's `http` span declares the fields).
    let span = tracing::Span::current();
    span.record("user_id", tracing::field::display(identity.user_id));
    span.record("client_id", tracing::field::display(identity.client_id));
    Ok(identity)
}

impl<S> FromRequestParts<S> for Identity
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        if let Some(identity) = parts.extensions.get::<Identity>() {
            return Ok(identity.clone());
        }
        let state = AppState::from_ref(state);
        let header = parts.headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());
        let identity = authenticate_header(&state, header).await?;
        parts.extensions.insert(identity.clone());
        Ok(identity)
    }
}

/// Require `scope` (403 `insufficient_scope`), then apply the per-token rate limit (429): routes
/// requiring `events:write` use the events bucket (600/min), all others the default (120/min).
/// Call once per request, first thing in the handler.
pub async fn require_scope(state: &AppState, identity: &Identity, scope: &str) -> Result<(), ApiError> {
    identity.ensure(scope)?;
    let (bucket, limit) = if scope == EVENTS_WRITE {
        ("events", state.settings.rate_limit_events_per_min)
    } else {
        ("default", state.settings.rate_limit_default_per_min)
    };
    check_rate_limit(&state.db.app, &format!("token:{}:{bucket}", identity.token_id), limit).await
}
