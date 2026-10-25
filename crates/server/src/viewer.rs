//! The web viewer: a static single-page app at `/viewer` for browsing, searching and forgetting
//! memories with a bearer token. It holds no data of its own; every call it makes is the same
//! `/v1` API the clients use, so access control stays in the API and the database.
//!
//! Three files, no inline script or style, so the content security policy can forbid both. The page
//! renders memory text, which comes from tool output and must be treated as untrusted: `app.js`
//! builds every node with `textContent`.

use axum::Router;
use axum::http::{HeaderName, HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;

const INDEX_HTML: &str = include_str!("../assets/viewer/index.html");
const APP_JS: &str = include_str!("../assets/viewer/app.js");
const APP_CSS: &str = include_str!("../assets/viewer/app.css");

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; \
                   base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// `/` and `/viewer` (the page), `/viewer/app.js`, `/viewer/app.css`.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/", get(|| async { Redirect::temporary("/viewer") }))
        .route("/viewer", get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }))
        .route("/viewer/", get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }))
        .route("/viewer/app.js", get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }))
        .route("/viewer/app.css", get(|| async { asset("text/css; charset=utf-8", APP_CSS) }))
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let headers: [(HeaderName, HeaderValue); 6] = [
        (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
        (header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP)),
        (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        (header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
        (header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY")),
        // The files are small and change with the binary; always revalidate.
        (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
    ];
    (headers, body).into_response()
}
