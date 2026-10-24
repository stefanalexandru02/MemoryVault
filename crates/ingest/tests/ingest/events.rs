//! Spec 6.2 event ingest and 9.1 window assignment.

use axum::http::StatusCode;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::schemas::events::{FIELD_HEAD_CHARS, FIELD_TAIL_CHARS};
use memvault_core::test_support::seed_identity;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{Ctx, PROJECT, ctx, ev, prompt, stop, tool};

/// `(seq, window_no)` of every event of the session.
async fn windows_of(c: &Ctx, session: Uuid) -> Vec<(i32, Option<i32>)> {
    sqlx::query_as("SELECT seq, window_no FROM events WHERE session_id = $1 ORDER BY seq")
        .bind(session)
        .fetch_all(&c.pool)
        .await
        .unwrap()
}

/// `(window_no, close_reason)` of every window, open ones with `None`.
async fn closes(c: &Ctx, session: Uuid) -> Vec<(i32, Option<String>)> {
    sqlx::query_as("SELECT window_no, close_reason FROM session_windows WHERE session_id = $1 ORDER BY window_no")
        .bind(session)
        .fetch_all(&c.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn eighth_tool_use_closes_the_window(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;

    let out = c.events_ok(&alice.token, s, (1..=7).map(|i| tool(s, i)).collect()).await;
    assert_eq!(out, json!({"accepted": 7, "duplicates": 0, "window_no": 1, "window_closed": false}));
    assert!(c.packets(s).await.is_empty());

    let out = c.events_ok(&alice.token, s, vec![tool(s, 8)]).await;
    assert_eq!((out["window_no"].clone(), out["window_closed"].clone()), (json!(1), json!(true)));
    assert_eq!(c.packets(s).await, vec![("extract".into(), format!("extract:{s}:1"), "pending".into())]);

    let out = c.events_ok(&alice.token, s, vec![tool(s, 9)]).await;
    assert_eq!((out["window_no"].clone(), out["window_closed"].clone()), (json!(2), json!(false)));

    // tool_failure counts as a tool use; one batch can close several windows
    let mut batch: Vec<Value> = (10..=16).map(|i| tool(s, i)).collect();
    batch.push(ev(s, 17, "tool_failure", json!({"tool_name": "Bash", "error": "exit 1"})));
    batch.extend((18..=25).map(|i| tool(s, i)));
    let out = c.events_ok(&alice.token, s, batch).await;
    assert_eq!((out["window_no"].clone(), out["window_closed"].clone()), (json!(4), json!(true)));

    let got = windows_of(&c, s).await;
    let expect: Vec<(i32, Option<i32>)> = (1..=25)
        .map(|seq| {
            (
                seq,
                Some(if seq <= 8 {
                    1
                } else if seq <= 16 {
                    2
                } else if seq <= 24 {
                    3
                } else {
                    4
                }),
            )
        })
        .collect();
    assert_eq!(got, expect);
    assert_eq!(
        closes(&c, s).await,
        vec![(1, Some("tool_uses".into())), (2, Some("tool_uses".into())), (3, Some("tool_uses".into())), (4, None)]
    );
    let keys: Vec<String> = c.packets(s).await.into_iter().map(|p| p.1).collect();
    assert_eq!(keys, (1..=3).map(|w| format!("extract:{s}:{w}")).collect::<Vec<_>>());

    // extract inputs carry the window's events, oldest first
    let inputs: Value = sqlx::query_scalar("SELECT inputs FROM work_packets WHERE idempotency_key = $1")
        .bind(format!("extract:{s}:2"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(inputs["window_no"], 2);
    assert_eq!(inputs["session"]["project_key"], PROJECT);
    assert_eq!(inputs["session"]["device"], alice.device_id);
    let events = inputs["events"].as_array().unwrap();
    assert_eq!(events.iter().map(|e| e["seq"].as_i64().unwrap()).collect::<Vec<_>>(), (9..=16).collect::<Vec<_>>());
    assert_eq!(events[0]["tool"], "Edit");
    assert_eq!(events[0]["input"], json!({"file_path": "src/f9.rs"}));
    assert!(events[0].get("text").is_none(), "null fields are dropped");
}

#[sqlx::test(migrations = "../../migrations")]
async fn stop_closes_the_window_and_owes_a_prompt_summary(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;

    // out of order within the batch: processed by seq
    let out =
        c.events_ok(&alice.token, s, vec![tool(s, 3), stop(s, 2, "done"), prompt(s, 0, "fix it"), tool(s, 1)]).await;
    assert_eq!(out["window_closed"], true);
    assert_eq!(out["window_no"], 2, "seq 3 went to the next window");
    assert_eq!(windows_of(&c, s).await, vec![(0, Some(1)), (1, Some(1)), (2, Some(1)), (3, Some(2))]);
    assert_eq!(closes(&c, s).await, vec![(1, Some("stop".into())), (2, None)]);

    // the prompt summary waits for the window's extract packet
    assert_eq!(c.packets(s).await, vec![("extract".into(), format!("extract:{s}:1"), "pending".into())]);
    let (pending, prompt_no): (bool, i32) = sqlx::query_as(
        "SELECT prompt_summary_pending, prompt_no FROM session_windows WHERE session_id = $1 AND window_no = 1",
    )
    .bind(s)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(pending);
    assert_eq!(prompt_no, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn prompt_only_windows_create_no_packet(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;
    let out = c
        .events_ok(&alice.token, s, vec![ev(s, 0, "system", json!({"source": "startup"})), prompt(s, 1, "hello")])
        .await;
    assert_eq!(out["window_closed"], false);
    let resp = c.post(&format!("/v1/sessions/{s}/end"), &alice.token, json!({})).await;
    assert_eq!(resp.json()["window_closed"], true);
    assert_eq!(resp.json()["packets_created"], json!([]));
    assert!(c.packets(s).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn duplicate_events_are_reported_not_reinserted(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;
    let batch: Vec<Value> = (1..=5).map(|i| tool(s, i)).collect();
    assert_eq!(c.events_ok(&alice.token, s, batch.clone()).await["accepted"], 5);

    // a replayed spool: everything is a duplicate, windows are unchanged
    let out = c.events_ok(&alice.token, s, batch).await;
    assert_eq!(out, json!({"accepted": 0, "duplicates": 5, "window_no": 1, "window_closed": false}));

    // same seq under a new key, and a key repeated inside one batch
    let mut same_seq = tool(s, 3);
    same_seq["idempotency_key"] = json!("other-key");
    let out = c.events_ok(&alice.token, s, vec![same_seq, tool(s, 6), tool(s, 6)]).await;
    assert_eq!((out["accepted"].clone(), out["duplicates"].clone()), (json!(1), json!(2)));

    let (events, tool_uses): (i64, i32) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM events WHERE session_id = $1),
                (SELECT tool_uses FROM session_windows WHERE session_id = $1 AND window_no = 1)",
    )
    .bind(s)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((events, tool_uses), (6, 6));
}

#[sqlx::test(migrations = "../../migrations")]
async fn payloads_are_clamped_and_token_estimated(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;
    let long = "x".repeat(FIELD_HEAD_CHARS + FIELD_TAIL_CHARS + 500);
    let event = ev(s, 1, "tool_use", json!({"tool_name": "Read", "tool_response": {"content": long}}));
    c.events_ok(&alice.token, s, vec![event]).await;
    let (payload, tokens): (Value, i32) =
        sqlx::query_as("SELECT payload, token_estimate FROM events WHERE session_id = $1")
            .bind(s)
            .fetch_one(&pool)
            .await
            .unwrap();
    let content = payload["tool_response"]["content"].as_str().unwrap();
    assert!(content.contains("…[clamped 500 chars]…"));
    assert_eq!(content.chars().count(), FIELD_HEAD_CHARS + FIELD_TAIL_CHARS + "…[clamped 500 chars]…".chars().count());
    assert_eq!(tokens, i32::try_from(payload.to_string().chars().count().div_ceil(4)).unwrap());
}

#[sqlx::test(migrations = "../../migrations")]
async fn batch_validation_and_ownership(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;

    let resp = c.events(&alice.token, s, vec![]).await;
    assert_eq!(resp.status, StatusCode::UNPROCESSABLE_ENTITY);
    let resp = c.events(&alice.token, s, (0..201).map(|i| tool(s, i)).collect()).await;
    assert_eq!(resp.status, StatusCode::UNPROCESSABLE_ENTITY);
    let resp = c.events(&alice.token, s, vec![ev(s, 1, "bogus_kind", json!({}))]).await;
    assert_eq!(resp.status, StatusCode::UNPROCESSABLE_ENTITY);
    let resp = c.events(&alice.token, Uuid::now_v7(), vec![tool(s, 1)]).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);

    // the same user's other device may not write into this client's session
    let (_, other) = crate::common::second_client(&pool, &alice, "desktop", TokenKind::Device).await;
    let resp = c.events(&other, s, vec![tool(s, 1)]).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);

    // body over 4 MB
    let huge = "y".repeat(4 * 1024 * 1024 + 10);
    let resp = c.events(&alice.token, s, vec![ev(s, 1, "note", json!({ "text": huge }))]).await;
    assert_eq!(resp.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", resp.text());
}
