//! The HTTP surface: scopes, status codes, content types, ETag.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use memvault_core::auth::scopes::TokenKind;
use memvault_core::test_support::{SeedOptions, app, seed_identity_with, send, send_request};
use serde_json::json;
use sqlx::PgPool;

use crate::common::*;

#[sqlx::test(migrations = "../../migrations")]
async fn recall_routes(pool: PgPool) {
    let w = world(pool).await;
    let obs =
        insert_obs(&w, Obs::new(w.alice.space_id, "Chose axum for the api", "axum router with tower layers")).await;
    let app = app(w.state.clone(), memvault_recall::router());
    let token = w.alice.token.as_str();

    let body = json!({"query": "axum router", "scopes": [{"kind": "global"}, {"kind": "project", "key": PROJECT}]});
    let resp = send(&app, Method::POST, "/v1/recall", Some(token), Some(body.clone())).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let v = resp.json();
    assert_eq!(v["slots"]["observations"][0]["id"], json!(obs));
    assert_eq!(v["slots"]["observations"][0]["type"], "discovery");
    assert!(v["rendered_markdown"].as_str().unwrap().contains("Chose axum"));
    assert!(v["tokens"]["by_slot"]["observations"].as_i64().unwrap() > 0);
    let rid = v["retrieval_id"].as_str().unwrap().to_string();
    let json_only = json!({"query": "axum", "format": "json"});
    let v = send(&app, Method::POST, "/v1/recall", Some(token), Some(json_only)).await.json();
    assert!(v["rendered_markdown"].is_null());

    let fb = json!({"used_ids": [obs], "helpful": true});
    let resp = send(&app, Method::POST, &format!("/v1/recall/{rid}/feedback"), Some(token), Some(fb)).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["recorded"], true);

    // Auth and scopes.
    assert_eq!(send(&app, Method::POST, "/v1/recall", None, Some(body.clone())).await.status, StatusCode::UNAUTHORIZED);
    let opts = SeedOptions { scopes: Some(vec!["events:write".into()]), ..SeedOptions::default() };
    let writer = seed_identity_with(&w.pool, TokenKind::Device, opts).await;
    let resp = send(&app, Method::POST, "/v1/recall", Some(&writer.token), Some(body)).await;
    assert_eq!((resp.status, resp.error_code().as_deref()), (StatusCode::FORBIDDEN, Some("insufficient_scope")));
    let runner = seed_identity_with(&w.pool, TokenKind::Runner, SeedOptions::default()).await;
    let resp =
        send(&app, Method::POST, &format!("/v1/recall/{rid}/feedback"), Some(&runner.token), Some(json!({}))).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "feedback needs memories:write");
    let bad = json!({"expand_top_observations": 99});
    assert_eq!(send(&app, Method::POST, "/v1/recall", Some(token), Some(bad)).await.status, 422);
}

#[sqlx::test(migrations = "../../migrations")]
async fn memory_routes(pool: PgPool) {
    let w = world(pool).await;
    let mut o = Obs::new(w.alice.space_id, "Picked sqlx runtime queries", "no compile-time macros");
    o.obs_type = "decision";
    o.files = vec!["crates/recall/src/engine.rs".into()];
    let obs = insert_obs(&w, o).await;
    let app = app(w.state.clone(), memvault_recall::router());
    let token = w.alice.token.as_str();

    let resp =
        send(&app, Method::GET, "/v1/search?q=sqlx%20queries&types=decision,bugfix&limit=5", Some(token), None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let v = resp.json();
    assert_eq!(v["rows"][0]["id"], json!(obs));
    assert_eq!(v["rows"][0]["table"], "observations");
    assert_eq!(v["rows"][0]["type"], "decision");
    assert_eq!(v["limit"], 5);
    let scoped = format!("/v1/search?scopes=project:{PROJECT}&tables=observations,facts");
    assert_eq!(send(&app, Method::GET, &scoped, Some(token), None).await.json()["rows"][0]["id"], json!(obs));
    let resp = send(&app, Method::GET, "/v1/search", Some(token), None).await;
    assert_eq!((resp.status.as_u16(), resp.error_code().as_deref()), (422, Some("invalid_request")));

    let resp = send(&app, Method::GET, &format!("/v1/timeline?anchor_id={obs}&before=2"), Some(token), None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["rows"][0]["is_anchor"], true);

    let resp = send(&app, Method::POST, "/v1/memories/get", Some(token), Some(json!({"ids": [obs]}))).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["items"][0]["table"], "observations");
    let resp = send(&app, Method::POST, "/v1/memories/get", Some(token), Some(json!({"ids": []}))).await;
    assert_eq!(resp.status, 422);

    let resp = send(&app, Method::GET, &format!("/v1/memories/{obs}/explain"), Some(token), None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["table"], "observations");
    let unknown = uuid::Uuid::now_v7();
    let resp = send(&app, Method::GET, &format!("/v1/memories/{unknown}/explain"), Some(token), None).await;
    assert_eq!((resp.status, resp.error_code().as_deref()), (StatusCode::NOT_FOUND, Some("not_found")));

    let uri = format!("/v1/observations/by-file?path=crates/recall/src/engine.rs&project_key={PROJECT}");
    let resp = send(&app, Method::GET, &uri, Some(token), None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["rows"][0]["id"], json!(obs));
}

#[sqlx::test(migrations = "../../migrations")]
async fn as_of_and_history_routes(pool: PgPool) {
    let w = world(pool).await;
    let h = insert_editor_history(&w).await;
    let app = app(w.state.clone(), memvault_recall::router());
    let token = w.alice.token.as_str();

    let uri = "/v1/search?q=editor&tables=facts&as_of=2026-02-01T00:00:00Z";
    let resp = send(&app, Method::GET, uri, Some(token), None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["rows"][0]["id"], json!(h.vim));
    // as_of alone is a filter, so a q-less search is valid.
    let resp = send(&app, Method::GET, "/v1/search?tables=facts&as_of=2026-04-01T00:00:00Z", Some(token), None).await;
    assert_eq!(resp.json()["rows"][0]["id"], json!(h.helix));
    for bad in ["yesterday", "2026-02-01", ""] {
        let resp = send(&app, Method::GET, &format!("/v1/search?q=x&as_of={bad}"), Some(token), None).await;
        assert_eq!((resp.status.as_u16(), resp.error_code().as_deref()), (422, Some("invalid_request")), "{bad}");
    }
    let body = json!({"query": "editor", "as_of": "2026-02-01T00:00:00Z", "rerank": false, "budgets": {"profile": 0}});
    let resp = send(&app, Method::POST, "/v1/recall", Some(token), Some(body)).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["slots"]["facts"][0]["id"], json!(h.vim));
    let resp = send(&app, Method::POST, "/v1/recall", Some(token), Some(json!({"as_of": "soon"}))).await;
    assert_eq!((resp.status.as_u16(), resp.error_code().as_deref()), (422, Some("invalid_request")));

    let resp = send(&app, Method::GET, &format!("/v1/memories/{}/history", h.helix), Some(token), None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let v = resp.json();
    assert_eq!((v["id"].clone(), v["table"].clone()), (json!(h.helix), json!("facts")));
    assert_eq!(v["entries"][0]["id"], json!(h.vim));
    assert_eq!(v["entries"][0]["ended_by"], "planner");
    assert_eq!(v["entries"][1]["current"], true);
    let unknown = uuid::Uuid::now_v7();
    let resp = send(&app, Method::GET, &format!("/v1/memories/{unknown}/history"), Some(token), None).await;
    assert_eq!((resp.status, resp.error_code().as_deref()), (StatusCode::NOT_FOUND, Some("not_found")));
    let uri = format!("/v1/memories/{}/history", h.helix);
    assert_eq!(send(&app, Method::GET, &uri, None, None).await.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations")]
async fn context_route_is_markdown_with_etag(pool: PgPool) {
    let w = world(pool).await;
    insert_obs(&w, Obs::new(w.alice.space_id, "Chose pgvector", "one database")).await;
    let app = app(w.state.clone(), memvault_recall::router());
    let token = w.alice.token.as_str();
    let uri = format!("/v1/context?project_key={PROJECT}&source=startup");

    let resp = send(&app, Method::GET, &uri, Some(token), None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.headers[header::CONTENT_TYPE], "text/markdown; charset=utf-8");
    assert!(resp.text().starts_with("## Memory\n"));
    assert!(resp.text().contains("Chose pgvector"));
    let etag = resp.headers[header::ETAG].to_str().unwrap().to_string();

    let req = Request::builder()
        .uri(&uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::IF_NONE_MATCH, &etag)
        .body(Body::empty())
        .unwrap();
    let resp = send_request(&app, req).await;
    assert_eq!(resp.status, StatusCode::NOT_MODIFIED);
    assert_eq!(resp.headers[header::ETAG], etag.as_str());
    assert!(resp.body.is_empty());

    let resume = format!("/v1/context?project_key={PROJECT}&source=resume");
    let resp = send(&app, Method::GET, &resume, Some(token), None).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.is_empty());
    let resp = send(&app, Method::GET, "/v1/context?source=bogus", Some(token), None).await;
    assert_eq!(resp.status, 422);
}
