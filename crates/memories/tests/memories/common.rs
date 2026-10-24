//! Shared setup: a world with Alice's device token and the REST routers of memories + recall.

use axum::Router;
use axum::http::StatusCode;
use memvault_core::AppState;
use memvault_core::appliers::AppliersRegistry;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::test_support::{Seeded, TestResponse, app, seed_identity, send, test_settings, test_state_with};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

pub const PROJECT: &str = "github.com/alex/homelab";

pub struct World {
    pub pool: PgPool,
    pub state: AppState,
    pub alice: Seeded,
    pub api: Router,
}

pub async fn world(pool: PgPool) -> World {
    let (state, _tei) = test_state_with(pool.clone(), test_settings(), AppliersRegistry::default());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let api = app(state.clone(), memvault_memories::router().merge(memvault_recall::router()));
    World { pool, state, alice, api }
}

impl World {
    pub async fn set_project(&self, who: &Seeded, project: &str) {
        sqlx::query("UPDATE clients SET current_project_key = $2 WHERE id = $1")
            .bind(who.client_id)
            .bind(project)
            .execute(&self.pool)
            .await
            .expect("set project");
    }

    pub async fn post(&self, token: &str, path: &str, body: Value) -> TestResponse {
        send(&self.api, "POST".parse().unwrap(), &format!("/v1{path}"), Some(token), Some(body)).await
    }

    pub async fn get(&self, token: &str, path: &str) -> TestResponse {
        send(&self.api, "GET".parse().unwrap(), &format!("/v1{path}"), Some(token), None).await
    }

    pub async fn delete(&self, token: &str, path: &str) -> TestResponse {
        send(&self.api, "DELETE".parse().unwrap(), &format!("/v1{path}"), Some(token), None).await
    }

    pub async fn count(&self, sql: &str, id: Uuid) -> i64 {
        sqlx::query_scalar(sql).bind(id).fetch_one(&self.pool).await.expect("count")
    }
}

/// A 2xx response's JSON body.
pub fn ok(resp: &TestResponse) -> Value {
    assert!(resp.status.is_success(), "unexpected {}: {}", resp.status, resp.text());
    resp.json()
}

pub fn uuid_of(v: &Value, pointer: &str) -> Uuid {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no uuid at {pointer} in {v}"))
}

pub fn fact_body(predicate: &str, value: &str) -> Value {
    json!({"kind": "fact", "subject": {"kind": "user", "name": "alex"}, "predicate": predicate, "value_text": value})
}

pub fn assert_status(resp: &TestResponse, status: StatusCode, code: &str) {
    assert_eq!(resp.status, status, "{}", resp.text());
    assert_eq!(resp.error_code().as_deref(), Some(code), "{}", resp.text());
}
