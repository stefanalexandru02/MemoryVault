use axum::Router;
use axum::http::{Method, StatusCode};
use axum::routing::post;
use memvault_core::http::{Json, Path, Query};
use memvault_core::schemas::validate::OutputError;
use memvault_core::test_support::{app, send, test_state};
use memvault_core::{ApiError, AppState};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;

#[derive(Deserialize)]
struct Body {
    n: i64,
}

#[derive(Deserialize)]
struct Q {
    limit: Option<i64>,
}

fn router() -> Router<AppState> {
    Router::new().route(
        "/_t/echo/{id}",
        post(|Path(id): Path<uuid::Uuid>, Query(q): Query<Q>, Json(b): Json<Body>| async move {
            Json(json!({"id": id, "n": b.n, "limit": q.limit}))
        }),
    )
}

#[sqlx::test(migrations = "../../migrations")]
async fn error_shapes(pool: PgPool) {
    let app = app(test_state(pool), router());
    let r = send(&app, Method::GET, "/v1/does-not-exist", None, None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({"error": {"code": "not_found", "message": "Not Found", "retryable": false}}));

    let id = uuid::Uuid::now_v7();
    let ok = send(&app, Method::POST, &format!("/v1/_t/echo/{id}?limit=3"), None, Some(json!({"n": 1}))).await;
    assert_eq!(ok.json(), json!({"id": id, "n": 1, "limit": 3}));

    let bad = send(&app, Method::POST, &format!("/v1/_t/echo/{id}"), None, Some(json!({"n": "x"}))).await;
    assert_eq!((bad.status, bad.error_code().as_deref()), (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_request")));
    let bad = send(&app, Method::POST, "/v1/_t/echo/not-a-uuid", None, Some(json!({"n": 1}))).await;
    assert_eq!(bad.error_code().as_deref(), Some("invalid_request"));
    let bad = send(&app, Method::POST, &format!("/v1/_t/echo/{id}?limit=x"), None, Some(json!({"n": 1}))).await;
    assert_eq!((bad.status, bad.error_code().as_deref()), (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_request")));
    let r = send(&app, Method::GET, &format!("/v1/_t/echo/{id}"), None, None).await;
    assert_eq!((r.status, r.error_code().as_deref()), (StatusCode::METHOD_NOT_ALLOWED, Some("method_not_allowed")));
}

#[test]
fn api_error_body_and_conversions() {
    let e = ApiError::invalid("bad").retryable(true).with_extra("details", json!([1]));
    assert_eq!(
        e.body(),
        json!({"error": {"code": "invalid_request", "message": "bad", "retryable": true, "details": [1]}})
    );
    let e: ApiError = OutputError::Invalid("output does not match summary.v1: x".into()).into();
    assert_eq!(e.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e.body()["error"]["code"], "schema_invalid");
    assert_eq!(e.body()["error"]["retry_with"], "output does not match summary.v1: x");
    let e: ApiError = sqlx::Error::RowNotFound.into();
    assert_eq!(e.status, StatusCode::NOT_FOUND);
    let e: ApiError = anyhow::Error::new(ApiError::conflict("dup")).into();
    assert_eq!(e.code, "conflict");
}
