//! `GET`/`POST /v1/work-state`.

use axum::http::StatusCode;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::schemas::common::MembershipRole;
use memvault_core::test_support::{seed_identity, seed_shared_space};
use serde_json::json;
use sqlx::PgPool;

use crate::common::{PROJECT, assert_status, ok, world};

#[sqlx::test(migrations = "../../migrations")]
async fn last_write_per_key_wins_and_null_removes(pool: PgPool) {
    let w = world(pool).await;
    let scope = json!({"kind": "project", "key": PROJECT});
    let write = |fields: serde_json::Value| json!({"scope": scope, "list_name": "todo", "fields": fields});

    let first = ok(&w.post(&w.alice.token, "/work-state", write(json!({"item-1": "open", "item-2": "open"}))).await);
    assert_eq!(first["lists"][0]["fields"], json!({"item-1": "open", "item-2": "open"}));
    ok(&w.post(&w.alice.token, "/work-state", write(json!({"item-1": "done", "item-3": "open"}))).await);
    let third = ok(&w.post(&w.alice.token, "/work-state", write(json!({"item-2": null}))).await);
    assert_eq!(third["lists"][0]["fields"], json!({"item-1": "done", "item-3": "open"}));

    let scope_param = format!("/work-state?scope=project:{PROJECT}");
    let read = ok(&w.get(&w.alice.token, &scope_param).await);
    let lists = read["lists"].as_array().unwrap();
    assert_eq!(lists.len(), 1, "{read}");
    assert_eq!(lists[0]["list_name"], "todo");
    assert_eq!(lists[0]["scope"], json!({"kind": "project", "key": PROJECT}));
    assert_eq!(lists[0]["fields"], json!({"item-1": "done", "item-3": "open"}));
    // Three rows are stored; the fold happens on read.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM work_state").fetch_one(&w.pool).await.unwrap();
    assert_eq!(rows, 3);
}

#[sqlx::test(migrations = "../../migrations")]
async fn defaults_filter_by_list_and_scope(pool: PgPool) {
    let w = world(pool).await;
    w.set_project(&w.alice, PROJECT).await;
    // No scope: the current project; a second list lives in global.
    ok(&w.post(&w.alice.token, "/work-state", json!({"list_name": "todo", "fields": {"a": "open"}})).await);
    ok(&w
        .post(
            &w.alice.token,
            "/work-state",
            json!({"list_name": "ideas", "fields": {"b": "x"}, "scope": {"kind": "global", "key": ""}}),
        )
        .await);
    let stored: String = sqlx::query_scalar("SELECT scope_key FROM work_state WHERE list_name = 'todo'")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(stored, PROJECT);

    let all = ok(&w.get(&w.alice.token, "/work-state").await);
    assert_eq!(all["lists"].as_array().unwrap().len(), 2, "{all}");
    let one = ok(&w.get(&w.alice.token, "/work-state?list=ideas").await);
    assert_eq!(one["lists"].as_array().unwrap().len(), 1);
    assert_eq!(one["lists"][0]["list_name"], "ideas");
    let global_only = ok(&w.get(&w.alice.token, "/work-state?scope=global:").await);
    assert_eq!(global_only["lists"].as_array().unwrap().len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn validation_scopes_and_roles(pool: PgPool) {
    let w = world(pool).await;
    assert_status(
        &w.post(&w.alice.token, "/work-state", json!({"list_name": "todo", "fields": {}})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
    );
    let runner = seed_identity(&w.pool, TokenKind::Runner).await;
    assert_status(
        &w.post(&runner.token, "/work-state", json!({"list_name": "todo", "fields": {"a": 1}})).await,
        StatusCode::FORBIDDEN,
        "insufficient_scope",
    );
    // A reader of a shared space sees its lists but cannot write to it.
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let shared = seed_shared_space(&w.pool, "team", w.alice.user_id, &[(bob.user_id, MembershipRole::Reader)]).await;
    let write = json!({"list_name": "todo", "fields": {"a": "open"}, "space_id": shared});
    ok(&w.post(&w.alice.token, "/work-state", write.clone()).await);
    assert_status(&w.post(&bob.token, "/work-state", write).await, StatusCode::FORBIDDEN, "space_forbidden");
    let seen = ok(&w.get(&bob.token, &format!("/work-state?space_ids={shared}&scope=global:")).await);
    assert_eq!(seen["lists"][0]["fields"], json!({"a": "open"}));
    // Nobody else's lists leak.
    let carol = seed_identity(&w.pool, TokenKind::Device).await;
    assert!(ok(&w.get(&carol.token, "/work-state").await)["lists"].as_array().unwrap().is_empty());
}
