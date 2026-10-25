//! Shared setup: an app with the ingest router, the ingest appliers and test prompts.

use axum::Router;
use axum::http::{Method, StatusCode};
use chrono::Utc;
use memvault_core::auth::admin::ensure_client;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::auth::tokens::{MintOptions, mint_token};
use memvault_core::test_support::{Seeded, TEST_PEPPER, TestResponse, app, send, test_settings, test_state_with};
use memvault_core::{AppState, AppliersRegistry, Settings, system_tx};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

pub const PROJECT: &str = "github.com/acme/app";

const PROMPTS: [(&str, &str); 3] = [
    (
        "observer.code.yaml",
        "name: observer.code\nversion: v1\nsystem: You observe.\nuser_template: \"window {{ window_no }}: {{ events | tojson }}\"\noutput_schema: observer.v1\n",
    ),
    (
        "summary.prompt.yaml",
        "name: summary.prompt\nversion: v1\nsystem: Summarise the prompt.\nuser_template: \"prompt {{ prompt_no }}: {{ prompt }} / {{ observation_titles | tojson }}\"\noutput_schema: summary.v1\n",
    ),
    (
        "summary.session.yaml",
        "name: summary.session\nversion: v2\nsystem: Summarise the session.\nuser_template: \"{{ prompt_summaries | tojson }}\"\noutput_schema: summary.v1\nmodel_hint: opus\n",
    ),
];

pub struct Ctx {
    pub pool: PgPool,
    pub state: AppState,
    pub app: Router,
}

pub fn ctx(pool: PgPool) -> Ctx {
    ctx_with(pool, |_| {})
}

pub fn ctx_with(pool: PgPool, tweak: impl FnOnce(&mut Settings)) -> Ctx {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in PROMPTS {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let mut settings = test_settings();
    settings.prompts_dir = dir.path().to_path_buf();
    tweak(&mut settings);
    let mut appliers = AppliersRegistry::default();
    memvault_ingest::register_appliers(&mut appliers);
    let (state, _tei) = test_state_with(pool.clone(), settings, appliers);
    let app = app(state.clone(), memvault_ingest::router());
    Ctx { pool, state, app }
}

impl Ctx {
    pub async fn post(&self, uri: &str, token: &str, body: Value) -> TestResponse {
        send(&self.app, Method::POST, uri, Some(token), Some(body)).await
    }

    /// `POST /v1/sessions` for `who` in project [`PROJECT`]; returns the session id.
    pub async fn start_session(&self, who: &Seeded, external: &str) -> Uuid {
        let resp = self
            .post(
                "/v1/sessions",
                &who.token,
                json!({"external_session_id": external, "project_key": PROJECT, "cwd": "/src/app"}),
            )
            .await;
        assert!(resp.status.is_success(), "{}", resp.text());
        resp.json()["session_id"].as_str().unwrap().parse().unwrap()
    }

    pub async fn events(&self, token: &str, session: Uuid, events: Vec<Value>) -> TestResponse {
        self.post(&format!("/v1/sessions/{session}/events"), token, json!({ "events": events })).await
    }

    /// Post events and expect 202.
    pub async fn events_ok(&self, token: &str, session: Uuid, events: Vec<Value>) -> Value {
        let resp = self.events(token, session, events).await;
        assert_eq!(resp.status, StatusCode::ACCEPTED, "{}", resp.text());
        resp.json()
    }

    pub async fn lease(&self, token: &str, body: Value) -> Vec<Value> {
        let resp = self.post("/v1/work/lease", token, body).await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        resp.json()["packets"].as_array().unwrap().clone()
    }

    pub async fn result(&self, token: &str, packet: &str, output: Value) -> TestResponse {
        let meta =
            json!({"model": "claude-sonnet-test", "runner_kind": "claude-p", "runner": "laptop", "duration_ms": 5});
        self.post(&format!("/v1/work/{packet}/result"), token, json!({"output": output, "meta": meta})).await
    }

    /// `(kind, idempotency_key, status)` of the session's packets, in creation order.
    pub async fn packets(&self, session: Uuid) -> Vec<(String, String, String)> {
        sqlx::query_as(
            "SELECT kind, idempotency_key, status FROM work_packets WHERE session_id = $1 ORDER BY created_at, id",
        )
        .bind(session)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// Make packets old enough for any runner (past the 10-minute preferred-runner window).
    pub async fn age_packets(&self) {
        sqlx::query("UPDATE work_packets SET available_after = available_after - interval '11 minutes'")
            .execute(&self.pool)
            .await
            .unwrap();
    }
}

pub fn ev(session: Uuid, seq: i32, kind: &str, payload: Value) -> Value {
    json!({
        "seq": seq, "kind": kind, "occurred_at": Utc::now(),
        "idempotency_key": format!("{session}:{seq}"), "payload": payload,
    })
}

pub fn tool(session: Uuid, seq: i32) -> Value {
    ev(
        session,
        seq,
        "tool_use",
        json!({"tool_name": "Edit", "tool_input": {"file_path": format!("src/f{seq}.rs")}, "tool_response": "ok"}),
    )
}

pub fn prompt(session: Uuid, seq: i32, text: &str) -> Value {
    ev(session, seq, "user_prompt", json!({ "prompt": text }))
}

pub fn stop(session: Uuid, seq: i32, text: &str) -> Value {
    ev(session, seq, "assistant_message", json!({"last_assistant_message": text, "stop_hook_active": false}))
}

/// Another client (device) of `owner`'s user with a token of `kind`; returns `(client_id, token)`.
pub async fn second_client(pool: &PgPool, owner: &Seeded, device: &str, kind: TokenKind) -> (Uuid, String) {
    let mut tx = system_tx(pool).await.unwrap();
    let client = ensure_client(&mut tx, owner.user_id, device, kind.client_kind(), None, None).await.unwrap();
    let minted = mint_token(&mut tx, owner.user_id, client, kind, TEST_PEPPER, MintOptions::default()).await.unwrap();
    tx.commit().await.unwrap();
    (client, minted.token)
}

/// A valid `observer.v1` output citing `seqs`.
pub fn observer_output(title: &str, seqs: &[i32]) -> Value {
    json!({
        "skip": false,
        "observations": [{
            "type": "bugfix", "title": title, "subtitle": "in the parser", "narrative": "The parser dropped the last token.",
            "facts": ["parser.rs drops the last token"], "concepts": ["problem-solution"],
            "entities": ["parser"], "files": ["src/parser.rs"], "importance": 0.7, "source_event_seqs": seqs,
        }],
        "fact_candidates": [],
    })
}

impl Ctx {
    /// Like [`Ctx::start_session`] with a raw token.
    pub async fn start_session_as(&self, token: &str, external: &str) -> Uuid {
        let resp =
            self.post("/v1/sessions", token, json!({"external_session_id": external, "project_key": PROJECT})).await;
        assert!(resp.status.is_success(), "{}", resp.text());
        resp.json()["session_id"].as_str().unwrap().parse().unwrap()
    }
}
