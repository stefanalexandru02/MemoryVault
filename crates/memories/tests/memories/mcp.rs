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

/// The short id in a `Remembered ... `<id>`` confirmation.
fn id_in(text: &str) -> String {
    text.split('`').nth(1).unwrap_or_else(|| panic!("no id in {text}")).to_string()
}

/// The full id of the row (of any table) a short id ends.
async fn full_id(pool: &PgPool, short: &str) -> String {
    sqlx::query_scalar(
        "SELECT id::text FROM (SELECT id FROM observations UNION ALL SELECT id FROM facts
                               UNION ALL SELECT id FROM procedures) t
          WHERE replace(id::text, '-', '') LIKE '%' || $1",
    )
    .bind(short)
    .fetch_one(pool)
    .await
    .unwrap()
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
            "memory_remember",
            "memory_forget",
            "memory_feedback",
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
    assert_eq!(by_name("memory_search")["inputSchema"]["properties"]["as_of"]["type"], "string");
    assert_eq!(by_name("memory_get")["inputSchema"]["required"], json!(["ids"]));
    assert_eq!(by_name("memory_get")["inputSchema"]["properties"]["trace"]["type"], "boolean");
    assert_eq!(by_name("memory_feedback")["inputSchema"]["required"], json!(["id", "verdict"]));
    assert_eq!(by_name("memory_feedback")["inputSchema"]["properties"]["verdict"]["enum"].as_array().unwrap().len(), 3);
    assert_eq!(by_name("memory_get")["inputSchema"]["properties"]["ids"]["maxItems"], 20);
    assert_eq!(by_name("memory_timeline")["inputSchema"]["properties"]["before"]["maximum"], 10);
}

#[sqlx::test(migrations = "../../migrations")]
async fn tool_definitions_stay_within_budget(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let list = rpc(&app, Some(&w.alice.token), &[], request(2, "tools/list", json!({}))).await;
    let tools = list.json()["result"]["tools"].clone();
    // Every session pays for these definitions: they were 5,654 characters for nine tools.
    let size = serde_json::to_string(&tools).unwrap().chars().count();
    assert!(size <= 3_600, "tools/list is {size} characters; keep descriptions tight");
    let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(!names.contains(&"memory_explain") && !names.contains(&"memory_history"), "merged into memory_get");
}

#[sqlx::test(migrations = "../../migrations")]
async fn remember_search_timeline_get_trace_forget(pool: PgPool) {
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

    // Confirmations name the memory by its short id.
    assert_eq!(first_id.len(), 6, "{first}");
    let first_full = full_id(&w.pool, &first_id).await;

    // Stored the way the REST path stores them.
    let (extracted, client): (Value, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT extracted_by, written_by_client_id FROM observations WHERE id = $1::uuid")
            .bind(&first_full)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!(extracted, json!({"runner_kind": "explicit"}));
    assert_eq!(client, Some(alice.client_id));

    let (err, found) = call(&app, alice, "memory_search", json!({"query": "router firmware"})).await;
    assert!(!err, "{found}");
    assert!(found.contains(&first_id) && found.contains(&second_id), "{found}");
    assert!(!found.contains(&first_full), "tables show short ids: {found}");
    assert!(!found.contains("Next:") && !found.contains("Legend"), "no footer, no legend: {found}");

    let (err, timeline) =
        call(&app, alice, "memory_timeline", json!({"anchor_id": second_id, "before": 3, "after": 3})).await;
    assert!(!err, "{timeline}");
    assert!(timeline.contains(&format!("**▶ {second_id}**")) && timeline.contains(&first_id), "{timeline}");

    // The short id, the citation form and the full id all name the same row; one header line each.
    let (err, got) = call(
        &app,
        alice,
        "memory_get",
        json!({"ids": [first_id, format!("obs#{first_id}"), first_full, "ffffffff-ffff-ffff-ffff-ffffffffffff"]}),
    )
    .await;
    assert!(!err, "{got}");
    assert!(got.contains("reboot after firmware updates") && got.contains("Not found"), "{got}");
    assert!(got.contains(&format!("[obs#{first_id} · discovery · ")) && !got.contains(&first_full), "{got}");
    assert!(!got.contains("Provenance"), "no trace unless asked: {got}");

    let (err, traced) = call(&app, alice, "memory_get", json!({"ids": [first_id], "trace": true})).await;
    assert!(!err && traced.contains("reboot after firmware") && traced.contains("**Provenance**"), "{traced}");
    assert!(traced.contains("runner_kind=explicit") && traced.contains("obs.create"), "{traced}");

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
async fn as_of_search_and_trace_history(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let alice = &w.alice;
    let remember = |value: &'static str| {
        call(
            &app,
            alice,
            "memory_remember",
            json!({"kind": "fact", "content": value, "subject": "alex", "predicate": "prefers.editor", "scope": "global"}),
        )
    };
    let (_, first) = remember("vim").await;
    let (first_id, between) = (full_id(&w.pool, &id_in(&first)).await, chrono::Utc::now().to_rfc3339());
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let (_, second) = remember("helix").await;
    let second_id = full_id(&w.pool, &id_in(&second)).await;
    let (first_short, second_short) = (&first_id[first_id.len() - 6..], &second_id[second_id.len() - 6..]);

    // Facts are not in the default search; with as_of they are, as they stood then.
    let (err, now) = call(&app, alice, "memory_search", json!({"query": "vim helix"})).await;
    assert!(!err && !now.contains(second_short), "{now}");
    let (err, past) = call(&app, alice, "memory_search", json!({"query": "vim helix", "as_of": between})).await;
    assert!(!err && past.contains(first_short) && !past.contains(second_short), "{past}");
    let (_, present) =
        call(&app, alice, "memory_search", json!({"query": "vim helix", "as_of": chrono::Utc::now().to_rfc3339()}))
            .await;
    assert!(present.contains(second_short) && !present.contains(first_short), "{present}");
    for bad in ["yesterday", "2026-10-01", "7d"] {
        let (err, text) = call(&app, alice, "memory_search", json!({"query": "vim helix", "as_of": bad})).await;
        assert!(err && text.contains("error (invalid_request)") && text.contains("as_of"), "{bad}: {text}");
    }

    // The chain from either end, by full id or short citation id, oldest first with the audit reason.
    for reference in [first_id.clone(), second_id.clone(), format!("fact#{second_short}")] {
        let (err, chain) = call(&app, alice, "memory_get", json!({"ids": [reference], "trace": true})).await;
        assert!(!err && chain.contains("2 versions"), "{chain}");
        let (vim, helix) = (chain.find("prefers.editor: vim"), chain.find("prefers.editor: helix"));
        assert!(vim.is_some() && helix.is_some() && vim < helix, "{chain}");
        assert!(chain.contains("replaced by an explicit write"), "{chain}");
    }
    let missing = json!({"ids": ["ffffffff-ffff-ffff-ffff-ffffffffffff"], "trace": true});
    let (err, text) = call(&app, alice, "memory_get", missing).await;
    assert!(err && text.contains("not_found"), "{text}");
    // Another user cannot see the chain.
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let (err, text) = call(&app, &bob, "memory_get", json!({"ids": [first_id], "trace": true})).await;
    assert!(err && text.contains("not_found"), "{text}");
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
                "SELECT scope_kind::text || ':' || scope_key FROM observations
                  WHERE replace(id::text, '-', '') LIKE '%' || $1",
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
    assert!(err && text.contains("no memory matches zzzz"), "{text}");
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

#[sqlx::test(migrations = "../../migrations")]
async fn feedback_tool_records_a_verdict_once_and_explains_the_outcome(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let alice = &w.alice;
    let (_, fact) = call(
        &app,
        alice,
        "memory_remember",
        json!({"kind": "fact", "content": "neovim", "predicate": "prefers.editor", "scope": "global"}),
    )
    .await;
    let id = id_in(&fact);

    let (err, wrong) =
        call(&app, alice, "memory_feedback", json!({"id": id, "verdict": "wrong", "note": "it is helix"})).await;
    assert!(!err, "{wrong}");
    assert!(wrong.starts_with(&format!("Recorded wrong on facts `{id}`.")), "{wrong}");
    assert!(
        wrong.contains("Confidence is now 0.75.") && wrong.contains("queued for the consolidation planner"),
        "{wrong}"
    );

    // The short id from the context block works, and a repeat changes nothing.
    let short = &id[id.len() - 6..];
    let (err, again) = call(&app, alice, "memory_feedback", json!({"id": short, "verdict": "wrong"})).await;
    assert!(!err && again.contains("already gave wrong"), "{again}");

    // The result of memory_get carries the tag a model should read.
    let (_, got) = call(&app, alice, "memory_get", json!({"ids": [id]})).await;
    assert!(got.contains("· disputed]"), "{got}");
    let (_, explained) = call(&app, alice, "memory_get", json!({"ids": [id], "trace": true})).await;
    assert!(explained.contains("fact.feedback") && explained.contains("wrong: it is helix"), "{explained}");

    let (err, bad) = call(&app, alice, "memory_feedback", json!({"id": id, "verdict": "meh"})).await;
    assert!(err && bad.contains("invalid arguments"), "{bad}");
    let (err, missing) = call(&app, alice, "memory_feedback", json!({"id": "zzzz", "verdict": "helpful"})).await;
    assert!(err && missing.contains("not_found"), "{missing}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn documents_are_found_by_type_and_read_with_their_source(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let source = json!({
        "scope": {"kind": "project", "key": PROJECT}, "uri": "docs/runbook.md", "content_type": "markdown",
        "content_hash": "h1", "content": "# Runbook\n\n## Rollback\n\nRoll back the migration with the down script first.",
    });
    assert_eq!(w.post(&w.alice.token, "/sources", source).await.status, StatusCode::CREATED);

    let (is_error, found) =
        call(&app, &w.alice, "memory_search", json!({"query": "roll back migration", "types": ["document"]})).await;
    assert!(!is_error, "{found}");
    assert!(found.contains("docs/runbook.md#Runbook"), "{found}");
    let (_, other) = call(&app, &w.alice, "memory_search", json!({"query": "roll back migration"})).await;
    assert!(!other.contains("runbook.md"), "documents are searched on request: {other}");

    let id: String = sqlx::query_scalar("SELECT id::text FROM documents").fetch_one(&w.pool).await.unwrap();
    let (is_error, body) = call(&app, &w.alice, "memory_get", json!({"ids": [id]})).await;
    assert!(!is_error, "{body}");
    assert!(body.contains("source: docs/runbook.md#Runbook") && body.contains("down script"), "{body}");
}

/// A global note with the given id, inserted the way the observer would leave it.
async fn insert_note(w: &World, id: uuid::Uuid, title: &str, age_minutes: f64) {
    sqlx::query(
        "INSERT INTO observations (id, space_id, scope_kind, scope_key, type, title, narrative, extracted_by,
                                   content_hash, token_estimate, created_at)
         VALUES ($1, $2, 'global', '', 'discovery', $3, 'narrative', '{}', $1::text, 10,
                 now() - make_interval(secs => $4 * 60.0))",
    )
    .bind(id)
    .bind(w.alice.space_id)
    .bind(title)
    .bind(age_minutes)
    .execute(&w.pool)
    .await
    .expect("insert note");
}

#[sqlx::test(migrations = "../../migrations")]
async fn short_ids_lengthen_when_rows_collide_and_ambiguous_ones_are_errors(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let alice = &w.alice;
    // Two rows whose ids end alike: the 6-hex short id names both.
    let (a, b) = (uuid::Uuid::from_u128(0x1a1b2c3), uuid::Uuid::from_u128(0x2a1b2c3));
    insert_note(&w, a, "First of a pair", 20.0).await;
    insert_note(&w, b, "Second of a pair", 10.0).await;

    let (err, table) = call(&app, alice, "memory_timeline", json!({"anchor_id": a.to_string()})).await;
    assert!(!err, "{table}");
    assert!(table.contains("**▶ 1a1b2c3**") && table.contains("| 2a1b2c3 |"), "lengthened to tell them apart: {table}");

    for (name, args) in [
        ("memory_get", json!({"ids": ["a1b2c3"]})),
        ("memory_timeline", json!({"anchor_id": "obs#a1b2c3"})),
        ("memory_forget", json!({"id": "a1b2c3"})),
        ("memory_feedback", json!({"id": "a1b2c3", "verdict": "helpful"})),
    ] {
        let (err, text) = call(&app, alice, name, args).await;
        assert!(
            err && text.contains("ambiguous") && text.contains("1a1b2c3") && text.contains("2a1b2c3"),
            "{name}: {text}"
        );
    }

    // The longer ending resolves; next to an ambiguous one the rest of a get still comes back.
    let (err, got) = call(&app, alice, "memory_get", json!({"ids": ["2a1b2c3", "a1b2c3"]})).await;
    assert!(!err && got.contains("Second of a pair") && !got.contains("First of a pair"), "{got}");
    assert!(got.contains("a1b2c3 is ambiguous"), "{got}");
    let (err, got) = call(&app, alice, "memory_get", json!({"ids": ["1a1b2c3", "2a1b2c3"]})).await;
    assert!(!err && got.contains("[obs#1a1b2c3 ") && got.contains("[obs#2a1b2c3 "), "{got}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn search_folds_near_duplicates_into_the_newest(pool: PgPool) {
    let w = world(pool).await;
    let app = mcp_app(&w);
    let alice = &w.alice;
    for (id, title, age) in [
        (0x11_u128, "Cache warm-up fails on boot", 30.0),
        (0x22, "cache warm-up FAILS on boot!", 20.0),
        (0x33, "Cache warm-up fails on boot.", 10.0),
        (0x44, "Cache warm-up skipped in tests", 5.0),
    ] {
        insert_note(&w, uuid::Uuid::from_u128(0xabc000 + id), title, age).await;
    }
    let (err, table) = call(&app, alice, "memory_search", json!({"query": "cache warm-up"})).await;
    assert!(!err, "{table}");
    // The newest of the three, standing for the other two; the different title keeps its own row.
    assert!(table.contains("| Cache warm-up fails on boot. (+2 similar) |"), "{table}");
    assert_eq!(table.matches("fails on boot").count() + table.matches("FAILS on boot").count(), 1, "{table}");
    assert!(table.contains("Cache warm-up skipped in tests |") && !table.contains("skipped in tests (+"), "{table}");
    assert!(table.contains(&format!("| {} |", "abc033")), "{table}");
}
