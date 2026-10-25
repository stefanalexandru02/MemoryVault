//! The HTTP surface: scopes, status codes, content types, ETag.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use memvault_core::auth::scopes::TokenKind;
use memvault_core::test_support::{
    SeedOptions, TestResponse, app, seed_identity, seed_identity_with, send, send_request,
};
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
    // Observation rows carry the observer's importance (the file-context hook skips minor ones); other rows do not.
    assert!(resp.json()["rows"][0]["importance"].as_f64().is_some_and(|i| (0.0..=1.0).contains(&i)));
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

#[sqlx::test(migrations = "../../migrations")]
async fn context_route_takes_index_rows_and_types(pool: PgPool) {
    let w = world(pool).await;
    let mut decision = Obs::new(w.alice.space_id, "Chose pgvector over Chroma", "one database");
    decision.obs_type = "decision";
    insert_obs(&w, decision).await;
    insert_obs(&w, Obs::new(w.alice.space_id, "Webhook retries arrive every fifteen minutes", "provider")).await;
    let app = app(w.state.clone(), memvault_recall::router());
    let token = w.alice.token.as_str();
    let get = |query: &'static str| {
        let app = &app;
        async move {
            let uri = format!("/v1/context?project_key={PROJECT}{query}");
            send(app, Method::GET, &uri, Some(token), None).await
        }
    };

    let all = get("").await.text();
    assert!(all.contains("Chose pgvector") && all.contains("Webhook retries"), "{all}");
    let one = get("&rows=1").await.text();
    assert_eq!(one.matches("\n| ").count(), 2, "the header row and one: {one}");
    let decisions = get("&types=decision").await.text();
    assert!(decisions.contains("Chose pgvector") && !decisions.contains("Webhook retries"), "{decisions}");
    for bad in ["&rows=0", "&rows=31", "&rows=many", "&types=opinion"] {
        let resp = get(bad).await;
        assert_eq!((resp.status.as_u16(), resp.error_code().as_deref()), (422, Some("invalid_request")), "{bad}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn context_mode_query_and_user_settings(pool: PgPool) {
    let w = world(pool).await;
    insert_obs(&w, Obs::new(w.alice.space_id, "Chose pgvector", "one database")).await;
    let app = app(w.state.clone(), memvault_recall::router());
    let token = w.alice.token.as_str();
    let uri = format!("/v1/context?project_key={PROJECT}&source=startup");

    let settings = send(&app, Method::GET, "/v1/me/settings", Some(token), None).await;
    assert_eq!((settings.status, settings.json()), (StatusCode::OK, json!({"injection_mode": "full"})));
    assert!(send(&app, Method::GET, &uri, Some(token), None).await.text().contains("Chose pgvector"));

    // The user's default applies until the request names a mode.
    let put = send(&app, Method::PUT, "/v1/me/settings", Some(token), Some(json!({"injection_mode": "lean"}))).await;
    assert_eq!((put.status, put.json()), (StatusCode::OK, json!({"injection_mode": "lean"})));
    let get = send(&app, Method::GET, "/v1/me/settings", Some(token), None).await;
    assert_eq!(get.json(), json!({"injection_mode": "lean"}));
    let lean = send(&app, Method::GET, &uri, Some(token), None).await;
    assert!(lean.text().contains("### Memory tools") && !lean.text().contains("Chose pgvector"), "{}", lean.text());
    let full = send(&app, Method::GET, &format!("{uri}&mode=full"), Some(token), None).await;
    assert!(full.text().contains("Chose pgvector") && !full.text().contains("### Memory tools"), "{}", full.text());
    assert_ne!(lean.headers[header::ETAG], full.headers[header::ETAG]);

    // Validation and scopes.
    let resp = send(&app, Method::GET, &format!("{uri}&mode=bogus"), Some(token), None).await;
    assert_eq!(resp.status, 422);
    for body in [json!({}), json!({"injection_mode": "fat"})] {
        let resp = send(&app, Method::PUT, "/v1/me/settings", Some(token), Some(body)).await;
        assert_eq!((resp.status.as_u16(), resp.error_code().as_deref()), (422, Some("invalid_request")));
    }
    assert_eq!(send(&app, Method::GET, "/v1/me/settings", None, None).await.status, StatusCode::UNAUTHORIZED);
    let opts = SeedOptions { scopes: Some(vec!["memories:read".into()]), ..SeedOptions::default() };
    let reader = seed_identity_with(&w.pool, TokenKind::Device, opts).await;
    let resp =
        send(&app, Method::PUT, "/v1/me/settings", Some(&reader.token), Some(json!({"injection_mode": "full"}))).await;
    assert_eq!((resp.status, resp.error_code().as_deref()), (StatusCode::FORBIDDEN, Some("insufficient_scope")));
    // Settings are per user.
    let other = send(&app, Method::GET, "/v1/me/settings", Some(&reader.token), None).await;
    assert_eq!(other.json(), json!({"injection_mode": "full"}));
}

#[sqlx::test(migrations = "../../migrations")]
async fn recall_stats_route(pool: PgPool) {
    let w = world(pool).await;
    insert_obs(&w, Obs::new(w.alice.space_id, "Chose axum for the api", "axum router with tower layers")).await;
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let admin = seed_identity(&w.pool, TokenKind::Admin).await;
    let app = app(w.state.clone(), memvault_recall::router());
    let token = w.alice.token.as_str();

    let empty = stats(&app, token, "").await;
    assert_eq!((empty.status, empty.json()), (StatusCode::OK, json!({"days": 7, "all_users": false, "modes": []})));

    // Alice: two full contexts (one render, one hit), one lean, one recall.
    let ctx = format!("/v1/context?project_key={PROJECT}");
    for suffix in ["", "", "&mode=lean"] {
        assert_eq!(send(&app, Method::GET, &format!("{ctx}{suffix}"), Some(token), None).await.status, 200);
    }
    let body = json!({"query": "axum router", "scopes": [{"kind": "global"}, {"kind": "project", "key": PROJECT}]});
    assert_eq!(send(&app, Method::POST, "/v1/recall", Some(token), Some(body)).await.status, 200);
    // Bob has no memories but is counted for himself and for the admin view.
    assert_eq!(send(&app, Method::GET, &ctx, Some(&bob.token), None).await.status, 200);

    let v = stats(&app, token, "").await.json();
    let modes = v["modes"].as_array().unwrap();
    let keys: Vec<(&str, Option<&str>, i64)> = modes
        .iter()
        .map(|m| (m["mode"].as_str().unwrap(), m["injection"].as_str(), m["calls"].as_i64().unwrap()))
        .collect();
    assert_eq!(keys, [("context", Some("full"), 2), ("context", Some("lean"), 1), ("recall", None, 1)], "{v}");
    let full = &modes[0];
    assert!(full["avg_tokens"].as_f64().unwrap() > 0.0 && full["p50_tokens"].as_f64().unwrap() > 0.0, "{v}");
    assert!(full["p95_tokens"].as_f64().unwrap() >= full["p50_tokens"].as_f64().unwrap(), "{v}");
    assert!(modes[1]["avg_tokens"].as_f64().unwrap() > 0.0 && modes[2]["avg_hits"].as_f64().unwrap() > 0.0, "{v}");

    let bobs = stats(&app, &bob.token, "?days=1").await.json();
    assert_eq!((bobs["days"].clone(), bobs["modes"][0]["calls"].clone()), (json!(1), json!(1)));
    let all = stats(&app, &admin.token, "?all=true").await.json();
    assert_eq!(all["all_users"], true);
    let total: i64 = all["modes"].as_array().unwrap().iter().map(|m| m["calls"].as_i64().unwrap()).sum();
    assert_eq!(total, 5, "{all}");
    let own = stats(&app, &admin.token, "").await.json();
    assert_eq!(own["modes"], json!([]), "the admin sees their own calls unless asked for all");

    // Old rows without tokens are ignored; a window that excludes the calls is empty.
    sqlx::query("UPDATE retrieval_log SET tokens = NULL WHERE mode = 'recall'").execute(&w.pool).await.unwrap();
    assert_eq!(stats(&app, token, "").await.json()["modes"].as_array().unwrap().len(), 2);
    sqlx::query("UPDATE retrieval_log SET created_at = now() - interval '3 days'").execute(&w.pool).await.unwrap();
    assert_eq!(stats(&app, token, "?days=2").await.json()["modes"], json!([]));
    assert_eq!(stats(&app, token, "?days=7").await.json()["modes"].as_array().unwrap().len(), 2);

    // Only admin tokens may ask for everyone; the window is bounded.
    let resp = stats(&app, token, "?all=true").await;
    assert_eq!((resp.status, resp.error_code().as_deref()), (StatusCode::FORBIDDEN, Some("insufficient_scope")));
    for bad in ["?days=0", "?days=91"] {
        let resp = stats(&app, token, bad).await;
        assert_eq!((resp.status.as_u16(), resp.error_code().as_deref()), (422, Some("invalid_request")), "{bad}");
    }
}

async fn stats(app: &Router, token: &str, query: &str) -> TestResponse {
    send(app, Method::GET, &format!("/v1/stats/recall{query}"), Some(token), None).await
}
