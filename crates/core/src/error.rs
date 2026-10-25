//! API error type. Every error response body is
//! `{"error": {"code": str, "message": str, "retryable": bool, ...extra}}` (spec 6).

use std::fmt;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

/// Return from any handler or extractor; renders in the spec error shape.
///
/// `extra` keys are merged into the error object (e.g. `retry_with` for `schema_invalid`),
/// `headers` are added to the response (e.g. `Retry-After`).
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub extra: Map<String, Value>,
    pub headers: Vec<(HeaderName, HeaderValue)>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            retryable: false,
            extra: Map::new(),
            headers: Vec::new(),
        }
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_extra(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }

    pub fn with_header(mut self, name: HeaderName, value: impl AsRef<str>) -> Self {
        if let Ok(v) = HeaderValue::from_str(value.as_ref()) {
            self.headers.push((name, v));
        }
        self
    }

    /// 400 `invalid_request`.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    /// 422 `invalid_request`: a well-formed body that fails a field constraint.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", message)
    }

    /// 401 with `WWW-Authenticate: Bearer`. Codes: `unauthorized`, `invalid_token`,
    /// `token_revoked`, `token_expired`, `user_disabled`.
    pub fn unauthorized(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code, message).with_header(header::WWW_AUTHENTICATE, "Bearer")
    }

    /// 403 with a specific code (`insufficient_scope`, `space_forbidden`, `forbidden`).
    pub fn forbidden(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    /// 404 `not_found`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    /// 409 `conflict`.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    /// 500 `internal` (retryable). The message is shown to the client; keep details in logs.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message).retryable(true)
    }

    /// 501 `not_implemented` (stubs of modules not built yet).
    pub fn not_implemented(what: &str) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, "not_implemented", format!("{what} is not implemented yet"))
    }

    /// The JSON body.
    pub fn body(&self) -> Value {
        let mut err = Map::new();
        err.insert("code".into(), json!(self.code));
        err.insert("message".into(), json!(self.message));
        err.insert("retryable".into(), json!(self.retryable));
        for (k, v) in &self.extra {
            err.insert(k.clone(), v.clone());
        }
        json!({ "error": err })
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.status.as_u16(), self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut resp = (self.status, axum::Json(self.body())).into_response();
        for (name, value) in self.headers {
            resp.headers_mut().append(name, value);
        }
        resp
    }
}

/// Database errors: RLS/privilege violations -> 403, unique violations -> 409, missing row -> 404,
/// everything else -> 500 (logged).
impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::RowNotFound => Self::not_found("not found"),
            sqlx::Error::Database(db) => match db.code().as_deref() {
                Some("42501") => Self::forbidden("forbidden", "not permitted in this space"),
                Some("23505") => Self::conflict(db.message().to_string()),
                _ => {
                    tracing::error!(error = %e, "database error");
                    Self::internal("database error")
                }
            },
            sqlx::Error::PoolTimedOut | sqlx::Error::Io(_) => {
                tracing::error!(error = %e, "database unavailable");
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "db_unavailable", "database unavailable").retryable(true)
            }
            _ => {
                tracing::error!(error = %e, "database error");
                Self::internal("database error")
            }
        }
    }
}

impl From<crate::tei::TeiError> for ApiError {
    fn from(e: crate::tei::TeiError) -> Self {
        tracing::warn!(error = %e, "TEI error");
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "tei_unavailable", e.to_string()).retryable(true)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<ApiError>() {
            Ok(api) => api,
            Err(e) => {
                tracing::error!(error = ?e, "internal error");
                Self::internal("internal server error")
            }
        }
    }
}

fn rejection(status: StatusCode, text: String) -> ApiError {
    let code = match status {
        StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
        _ => "invalid_request",
    };
    ApiError::new(status, code, text)
}

impl From<JsonRejection> for ApiError {
    fn from(r: JsonRejection) -> Self {
        rejection(r.status(), r.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(r: QueryRejection) -> Self {
        rejection(StatusCode::UNPROCESSABLE_ENTITY, r.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(r: PathRejection) -> Self {
        rejection(r.status(), r.body_text())
    }
}
