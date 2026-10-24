//! Fixtures: a state with the consolidation appliers and prompts, and SQL seeding helpers.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use memvault_core::appliers::AppliedCounts;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::db::system_tx;
use memvault_core::ids::new_id;
use memvault_core::schemas::common::{OutputSchemaName, PacketKind};
use memvault_core::schemas::work::PacketRecord;
use memvault_core::tei::FakeTei;
use memvault_core::test_support::{Seeded, seed_identity, test_settings, test_state_with};
use memvault_core::{AppState, AppliersRegistry};
use pgvector::Vector;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub struct Ctx {
    pub pool: PgPool,
    pub state: AppState,
    pub tei: Arc<FakeTei>,
}

pub fn ctx(pool: PgPool) -> Ctx {
    let mut settings = test_settings();
    settings.prompts_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("prompts");
    let mut registry = AppliersRegistry::default();
    memvault_consolidate::register_appliers(&mut registry);
    let (state, tei) = test_state_with(pool.clone(), settings, registry);
    Ctx { pool, state, tei }
}

impl Ctx {
    pub async fn tx(&self) -> Transaction<'static, Postgres> {
        system_tx(&self.pool).await.expect("system_tx")
    }

    /// Insert a pending packet of `kind` with `inputs` and read it back.
    pub async fn packet(&self, space: Uuid, kind: PacketKind, inputs: Value) -> PacketRecord {
        let schema = match kind {
            PacketKind::ConsolidateFacts => OutputSchemaName::ConsolidateFactsV1,
            PacketKind::ConsolidateSummary => OutputSchemaName::ConsolidateSummaryV1,
            _ => OutputSchemaName::ConsolidateProceduresV1,
        };
        let id = new_id();
        sqlx::query(
            "INSERT INTO work_packets (id, space_id, kind, prompt_ref, model_hint, inputs, output_schema, idempotency_key)
             VALUES ($1, $2, $3, 'consolidate.test@v1', 'opus', $4, $5, $6)",
        )
        .bind(id)
        .bind(space)
        .bind(kind)
        .bind(&inputs)
        .bind(schema)
        .bind(format!("test:{id}"))
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query_as("SELECT * FROM work_packets WHERE id = $1").bind(id).fetch_one(&self.pool).await.unwrap()
    }

    /// Validate `output` against the packet's schema and apply it in one committed transaction.
    pub async fn apply(&self, packet: &PacketRecord, output: Value) -> AppliedCounts {
        self.try_apply(packet, output).await.expect("apply")
    }

    pub async fn try_apply(
        &self,
        packet: &PacketRecord,
        output: Value,
    ) -> Result<AppliedCounts, memvault_core::ApiError> {
        let typed = self.state.schemas.validate(packet.output_schema, &output).expect("output matches the schema");
        let mut tx = self.tx().await;
        let counts = self.state.appliers.apply(&mut tx, packet, &typed).await?;
        tx.commit().await.unwrap();
        Ok(counts)
    }
}

pub async fn seed(pool: &PgPool) -> Seeded {
    seed_identity(pool, TokenKind::Device).await
}

pub async fn entity(pool: &PgPool, space: Uuid, name: &str) -> Uuid {
    let id = new_id();
    sqlx::query("INSERT INTO entities (id, space_id, kind, canonical_name) VALUES ($1, $2, 'user', $3)")
        .bind(id)
        .bind(space)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}

#[derive(Clone)]
pub struct FactSpec {
    pub scope_kind: &'static str,
    pub scope_key: &'static str,
    pub predicate: &'static str,
    pub value_text: &'static str,
    pub status: &'static str,
    pub confidence: f64,
    pub importance: f64,
    pub conflict: bool,
    pub supersedes_id: Option<Uuid>,
    pub pinned: bool,
    pub age: Duration,
    pub evidence: Vec<Uuid>,
    pub confirmations: i32,
}

impl Default for FactSpec {
    fn default() -> Self {
        Self {
            scope_kind: "project",
            scope_key: "acme",
            predicate: "prefers.editor",
            value_text: "vim",
            status: "candidate",
            confidence: 0.6,
            importance: 0.5,
            conflict: false,
            supersedes_id: None,
            pinned: false,
            age: Duration::hours(48),
            evidence: Vec::new(),
            confirmations: 1,
        }
    }
}

pub async fn fact(pool: &PgPool, space: Uuid, subject: Uuid, s: FactSpec) -> Uuid {
    let id = new_id();
    let seen = Utc::now() - s.age;
    sqlx::query(
        "INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text, confidence,
                            importance, status, conflict, supersedes_id, pinned, first_seen_at, last_confirmed_at,
                            evidence_observation_ids, confirmations)
         VALUES ($1, $2, $3::scope_kind, $4, $5, $6, $7, $8, $9::float8::float4, $10::float8::float4, $11, $12, $13, $14,
                 $15, $15, $16, $17)",
    )
    .bind(id)
    .bind(space)
    .bind(s.scope_kind)
    .bind(s.scope_key)
    .bind(subject)
    .bind(s.predicate)
    .bind(json!(s.value_text))
    .bind(s.value_text)
    .bind(s.confidence)
    .bind(s.importance)
    .bind(s.status)
    .bind(s.conflict)
    .bind(s.supersedes_id)
    .bind(s.pinned)
    .bind(seen)
    .bind(&s.evidence)
    .bind(s.confirmations)
    .execute(pool)
    .await
    .unwrap();
    id
}

pub struct ObsSpec {
    pub scope_kind: &'static str,
    pub scope_key: &'static str,
    pub obs_type: &'static str,
    pub title: &'static str,
    pub importance: f64,
    pub created_at: DateTime<Utc>,
    pub session_id: Option<Uuid>,
}

impl Default for ObsSpec {
    fn default() -> Self {
        Self {
            scope_kind: "project",
            scope_key: "acme",
            obs_type: "change",
            title: "an observation",
            importance: 0.5,
            created_at: Utc::now(),
            session_id: None,
        }
    }
}

pub async fn observation(pool: &PgPool, space: Uuid, s: ObsSpec) -> Uuid {
    let id = new_id();
    sqlx::query(
        "INSERT INTO observations (id, space_id, scope_kind, scope_key, session_id, type, title, importance,
                                   content_hash, extracted_by, created_at)
         VALUES ($1, $2, $3::scope_kind, $4, $5, $6, $7, $8::float8::float4, $9, '{}'::jsonb, $10)",
    )
    .bind(id)
    .bind(space)
    .bind(s.scope_kind)
    .bind(s.scope_key)
    .bind(s.session_id)
    .bind(s.obs_type)
    .bind(s.title)
    .bind(s.importance)
    .bind(id.to_string())
    .bind(s.created_at)
    .execute(pool)
    .await
    .unwrap();
    id
}

pub async fn session(pool: &PgPool, space: Uuid, client: Uuid, project: &str) -> Uuid {
    let id = new_id();
    sqlx::query(
        "INSERT INTO sessions (id, space_id, client_id, harness, external_session_id, project_key)
         VALUES ($1, $2, $3, 'claude-code', $4, $5)",
    )
    .bind(id)
    .bind(space)
    .bind(client)
    .bind(id.to_string())
    .bind(project)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Store the `value` embedding of a fact as the worker would (FakeTei vector of `text`).
pub async fn embed_fact(c: &Ctx, fact_id: Uuid, text: &str) {
    let v = c.tei.vector(text);
    sqlx::query(
        "INSERT INTO embeddings (space_id, owner_table, owner_id, part, model, embedding, scope_kind, scope_key)
         SELECT space_id, 'facts', id, 'value', $2, $3, scope_kind, scope_key FROM facts WHERE id = $1",
    )
    .bind(fact_id)
    .bind(&c.state.settings.embed_model)
    .bind(Vector::from(v))
    .execute(&c.pool)
    .await
    .unwrap();
}

pub async fn text(pool: &PgPool, sql: &str, id: Uuid) -> String {
    sqlx::query_scalar(sql).bind(id).fetch_one(pool).await.unwrap()
}

pub async fn status_of(pool: &PgPool, fact: Uuid) -> String {
    text(pool, "SELECT status FROM facts WHERE id = $1", fact).await
}

pub async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// Audit actions of a run in the order they were written.
pub async fn audit_actions(pool: &PgPool, run_id: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM memory_audit WHERE run_id = $1 ORDER BY seq")
        .bind(run_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

pub fn run_inputs(run_id: Uuid, fact_ids: &[Uuid]) -> Value {
    json!({"run_id": run_id, "fact_ids": fact_ids})
}
