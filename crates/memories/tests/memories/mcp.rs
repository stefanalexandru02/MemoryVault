//! The MCP server over HTTP: initialize, tools/list, tools/call, auth.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use memvault_core::auth::scopes::TokenKind;
use memvault_core::test_support::{Seeded, TestResponse, seed_identity, send_request};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::common::{PROJECT, World, world};

/// The server mounts the service at `/mcp` exactly like this.
fn mcp_app(w: &World) -> Router {
    Router::new().nest_service("/mcp", memvault_memories::mcp::service(w.state.clone()))
}

async fn rpc(app: &Router, token: Option<&str>, headers: &[(&str, &str)], body: Value) -> TestResponse {
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "memory.tailnet.example")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream");
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    send_request(app, req.body(Body::from(body.to_string())).unwrap()).await
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// `tools/call`: `(is_error, text)`.
async fn call_with(app: &Router, who: &Seeded, headers: &[(&str, &str)], name: &str, args: Value) -> (bool, String) {
    let resp =
        rpc(app, Some(&who.token), headers, request(2, "tools/call", json!({"name": name, "arguments": args}))).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let body = resp.json();
    let result = body.get("result").unwrap_or_else(|| panic!("no result: {body}"));
    let text = result["content"][0]["text"].as_str().unwrap_or_default().to_string();
    (result["isError"].as_bool().unwrap_or(false), text)
}

async fn call(app: &Router, who: &Seeded, name: &str, args: Value) -> (bool, String) {
    call_with(app, who, &[], name, args).await
}

/// The id in a `Remembered ... `<id>`` confirmation.
fn id_in(text: &str) -> String {
    text.split('`').nth(1).unwrap_or_else(|| panic!("no id in {text}")).to_string()
}

#[sqlx::test(migrations = "../../migrations")]
async fn requires_a_valid_bearer_token(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let init = request(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    );

    let none = rpc(&app, None, &[], init.clone()).await;
    assert_eq!(none.status, StatusCode::UNAUTHORIZED, "{}", none.text());
    assert_eq!(none.error_code().as_deref(), Some("unauthorized"));
    assert!(none.headers.contains_key(header::WWW_AUTHENTICATE));
    let bad = rpc(&app, Some("mem_aaaaaaaa_notarealtoken"), &[], init.clone()).await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    assert_eq!(bad.error_code().as_deref(), Some("invalid_token"));
    // Every method needs the token, not just the first request.
    let list = rpc(&app, None, &[], request(2, "tools/list", json!({}))).await;
    assert_eq!(list.status, StatusCode::UNAUTHORIZED);

    let ok = rpc(&app, Some(&w.alice.token), &[], init).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text());
}

#[sqlx::test(migrations = "../../migrations")]
async fn initialize_and_tools_list(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let init = rpc(
        &app,
        Some(&w.alice.token),
        &[],
        request(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
        ),
    )
    .await;
    assert_eq!(init.status, StatusCode::OK, "{}", init.text());
    let result = init.json()["result"].clone();
    assert!(result["capabilities"]["tools"].is_object(), "{result}");
    assert_eq!(result["serverInfo"]["name"], "memvault");
    // Stateless: no session is handed out and none is needed afterwards.
    assert!(init.headers.get("mcp-session-id").is_none());

    // What Claude Code sends next: the initialized notification, then requests with the negotiated version.
    let initialized = rpc(
        &app,
        Some(&w.alice.token),
        &[("mcp-protocol-version", "2025-06-18")],
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(initialized.status, StatusCode::ACCEPTED, "{}", initialized.text());
    let list =
        rpc(&app, Some(&w.alice.token), &[("mcp-protocol-version", "2025-06-18")], request(2, "tools/list", json!({})))
            .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text());
    let tools = list.json()["result"]["tools"].as_array().unwrap().clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "memory_search",
            "memory_timeline",
            "memory_get",
            "memory_explain",
            "memory_remember",
            "memory_forget",
            "work_state_write"
        ]
    );
    for t in &tools {
        assert_eq!(t["_meta"]["anthropic/maxResultSizeChars"], 40000, "{t}");
        let description = t["description"].as_str().unwrap();
        assert!(!description.is_empty() && description.matches(". ").count() == 0, "one sentence: {description}");
        assert_eq!(t["inputSchema"]["type"], "object");
    }
    let by_name = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap().clone();
    for n in ["memory_search", "memory_timeline", "memory_get"] {
        assert!(by_name(n)["description"].as_str().unwrap().contains("search → timeline → get"), "{n}");
    }
    assert_eq!(by_name("memory_search")["inputSchema"]["required"], json!(["query"]));
    assert_eq!(by_name("memory_search")["inputSchema"]["properties"]["limit"]["maximum"], 30);
    assert_eq!(by_name("memory_get")["inputSchema"]["properties"]["ids"]["maxItems"], 20);
    assert_eq!(by_name("memory_timeline")["inputSchema"]["properties"]["before"]["maximum"], 10);
}

#[sqlx::test(migrations = "../../migrations")]
async fn remember_search_timeline_get_explain_forget(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let alice = &w.alice;

    let (err, first) = call(
        &app,
        alice,
        "memory_remember",
        json!({"kind": "note", "content": "The homelab router needs a reboot after firmware updates."}),
    )
    .await;
    assert!(!err, "{first}");
    let first_id = id_in(&first);
    let (_, second) = call(
        &app,
        alice,
        "memory_remember",
        json!({"kind": "note", "content": "Router firmware 4.2 fixed the DHCP lease bug."}),
    )
    .await;
    let second_id = id_in(&second);
    let (err, fact) = call(
        &app,
        alice,
        "memory_remember",
        json!({"kind": "fact", "content": "neovim", "subject": "alex", "predicate": "prefers.editor", "scope": "global"}),
    )
    .await;
    assert!(!err, "{fact}");
    assert!(fact.contains("prefers.editor: neovim"), "{fact}");
    let (_, proc) = call(
        &app,
        alice,
        "memory_remember",
        json!({"kind": "procedure", "content": "1. drain the node\n2. run the update", "subject": "Update the router", "predicate": "maintenance"}),
    )
    .await;
    assert!(proc.contains("Update the router"), "{proc}");

    // Stored the way the REST path stores them.
    let (extracted, client): (Value, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT extracted_by, written_by_client_id FROM observations WHERE id = $1::uuid")
            .bind(&first_id)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!(extracted, json!({"runner_kind": "explicit"}));
    assert_eq!(client, Some(alice.client_id));

    let (err, found) = call(&app, alice, "memory_search", json!({"query": "router firmware"})).await;
    assert!(!err, "{found}");
    assert!(found.contains(&first_id) && found.contains(&second_id), "{found}");
    assert!(found.contains("memory_timeline") && found.contains("memory_get"), "next-step hint: {found}");

    let (err, timeline) =
        call(&app, alice, "memory_timeline", json!({"anchor_id": second_id, "before": 3, "after": 3})).await;
    assert!(!err, "{timeline}");
    assert!(timeline.contains(&format!("**▶ {second_id}**")) && timeline.contains(&first_id), "{timeline}");

    // The short id shown in citations works too.
    let short = &first_id[first_id.len() - 6..];
    let (err, got) = call(
        &app,
        alice,
        "memory_get",
        json!({"ids": [first_id, format!("obs#{short}"), "ffffffff-ffff-ffff-ffff-ffffffffffff"]}),
    )
    .await;
    assert!(!err, "{got}");
    assert!(got.contains("reboot after firmware updates") && got.contains("Not found"), "{got}");

    let (err, explained) = call(&app, alice, "memory_explain", json!({"id": first_id})).await;
    assert!(!err && explained.contains("Provenance") && explained.contains("runner_kind=explicit"), "{explained}");

    let (err, gone) = call(&app, alice, "memory_forget", json!({"id": first_id, "reason": "obsolete"})).await;
    assert!(!err && gone.contains("archived"), "{gone}");
    let reason: Option<String> = sqlx::query_scalar("SELECT reason FROM memory_audit WHERE action = 'obs.archive'")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(reason.as_deref(), Some("obsolete"));
    let (_, after) = call(&app, alice, "memory_search", json!({"query": "router firmware"})).await;
    assert!(!after.contains(&first_id) && after.contains(&second_id), "{after}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn project_scope_is_inferred(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let alice = &w.alice;

    // Without a known project, "project" cannot be written.
    let (err, text) =
        call(&app, alice, "memory_remember", json!({"kind": "note", "content": "needs a project", "scope": "project"}))
            .await;
    assert!(err && text.contains("no current project"), "{text}");

    w.set_project(alice, PROJECT).await;
    let (_, project_note) = call(
        &app,
        alice,
        "memory_remember",
        json!({"kind": "note", "content": "Project note about the staging cluster"}),
    )
    .await;
    let (_, global_note) = call(
        &app,
        alice,
        "memory_remember",
        json!({"kind": "note", "content": "Global note about the staging cluster", "scope": "global"}),
    )
    .await;
    let (project_id, global_id) = (id_in(&project_note), id_in(&global_note));
    let scope_of = |id: String| {
        let pool = w.pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT scope_kind::text || ':' || scope_key FROM observations WHERE id = $1::uuid",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(scope_of(project_id.clone()).await, format!("project:{PROJECT}"));
    assert_eq!(scope_of(global_id.clone()).await, "global:");

    let search =
        |scope: &'static str| call(&app, alice, "memory_search", json!({"query": "staging cluster", "scope": scope}));
    let (_, in_project) = search("project").await;
    assert!(in_project.contains(&project_id) && !in_project.contains(&global_id), "{in_project}");
    let (_, in_global) = search("global").await;
    assert!(in_global.contains(&global_id) && !in_global.contains(&project_id), "{in_global}");
    let (_, in_all) = search("all").await;
    assert!(in_all.contains(&global_id) && in_all.contains(&project_id), "{in_all}");

    // The X-Memory-Project header, when the plugin sends it, wins over the stored project.
    let (_, other) = call_with(
        &app,
        alice,
        &[("x-memory-project", "github.com/alex/other")],
        "memory_remember",
        json!({"kind": "note", "content": "Note filed under another project"}),
    )
    .await;
    assert_eq!(scope_of(id_in(&other)).await, "project:github.com/alex/other");

    // Searching with the header and no explicit scope stays in that project; "all" opts out.
    let headers = [("x-memory-project", PROJECT)];
    let (_, narrowed) = call_with(&app, alice, &headers, "memory_search", json!({"query": "staging cluster"})).await;
    assert!(narrowed.contains(&project_id) && !narrowed.contains(&global_id), "{narrowed}");
    let args = json!({"query": "staging cluster", "scope": "all"});
    let (_, everything) = call_with(&app, alice, &headers, "memory_search", args).await;
    assert!(everything.contains(&project_id) && everything.contains(&global_id), "{everything}");

    // Work state follows the same project.
    let (err, wrote) =
        call(&app, alice, "work_state_write", json!({"list": "todo", "fields": {"item-1": "open"}})).await;
    assert!(!err && wrote.contains("todo") && wrote.contains("item-1"), "{wrote}");
    let key: String = sqlx::query_scalar("SELECT scope_key FROM work_state").fetch_one(&w.pool).await.unwrap();
    assert_eq!(key, PROJECT);
    let (_, removed) = call(&app, alice, "work_state_write", json!({"list": "todo", "fields": {"item-1": null}})).await;
    assert!(removed.contains("empty"), "{removed}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn bad_calls_are_tool_errors(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let alice = &w.alice;

    let (err, text) = call(&app, alice, "memory_search", json!({"query": "x", "limit": 31})).await;
    assert!(err && text.contains("limit"), "{text}");
    let (err, text) = call(&app, alice, "memory_search", json!({})).await;
    assert!(err && text.contains("query"), "{text}");
    let (err, text) = call(&app, alice, "memory_timeline", json!({"anchor_id": "abcdef", "before": 11})).await;
    assert!(err && text.contains("before"), "{text}");
    let ids: Vec<String> = (0..21).map(|_| uuid::Uuid::now_v7().to_string()).collect();
    let (err, text) = call(&app, alice, "memory_get", json!({"ids": ids})).await;
    assert!(err && text.contains("20"), "{text}");
    let (err, text) = call(&app, alice, "memory_remember", json!({"kind": "fact", "content": "no predicate"})).await;
    assert!(err && text.contains("predicate"), "{text}");
    let (err, text) = call(&app, alice, "memory_forget", json!({"id": "zzzz"})).await;
    assert!(err && text.contains("no single memory"), "{text}");
    let (err, text) = call(&app, alice, "no_such_tool", json!({})).await;
    assert!(err && text.contains("unknown tool"), "{text}");

    // A token without memories:write can read but not write.
    let runner = seed_identity(&w.pool, TokenKind::Runner).await;
    let (err, text) = call(&app, &runner, "memory_remember", json!({"kind": "note", "content": "nope"})).await;
    assert!(err && text.contains("insufficient_scope"), "{text}");
    let (err, _) = call(&app, &runner, "memory_search", json!({"query": "anything"})).await;
    assert!(!err);
}

#[sqlx::test(migrations = "../../migrations")]
async fn other_users_memories_stay_invisible(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let (_, note) =
        call(&app, &w.alice, "memory_remember", json!({"kind": "note", "content": "alice private zebra note"})).await;
    let id = id_in(&note);
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let (_, found) = call(&app, &bob, "memory_search", json!({"query": "zebra"})).await;
    assert!(!found.contains(&id), "{found}");
    let (err, _) = call(&app, &bob, "memory_get", json!({"ids": [id.clone()]})).await;
    assert!(err);
    let (err, _) = call(&app, &bob, "memory_forget", json!({"id": id})).await;
    assert!(err);
}

#[sqlx::test(migrations = "../../migrations")]
async fn get_truncates_long_bodies_to_the_cap(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let mut ids = Vec::new();
    for i in 0..20 {
        let narrative = format!("note {i} {}", "word ".repeat(390));
        let resp = w
            .post(
                &w.alice.token,
                "/memories",
                json!({"kind": "observation", "title": format!("Long note {i}"), "narrative": narrative}),
            )
            .await;
        ids.push(resp.json()["id"].as_str().unwrap().to_string());
    }
    let (err, text) = call(&app, &w.alice, "memory_get", json!({"ids": ids})).await;
    assert!(!err);
    assert!(text.chars().count() <= 24_000 + 20 * 100, "{} chars", text.chars().count());
    assert_eq!(text.matches("#### ").count(), 20, "every body is present, none dropped");
    assert!(text.contains("truncated"));
}
