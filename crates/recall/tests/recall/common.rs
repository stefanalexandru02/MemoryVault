//! Seeding helpers: rows inserted with SQL as the superuser, embeddings from FakeTei vectors.

use std::sync::Arc;

use memvault_core::AppState;
use memvault_core::appliers::AppliersRegistry;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::ids::new_id;
use memvault_core::schemas::common::{Scope, ScopeKind, Sensitivity, SummaryGrain};
use memvault_core::tei::FakeTei;
use memvault_core::test_support::{Seeded, seed_identity, test_settings, test_state_with};
use pgvector::Vector;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

pub const PROJECT: &str = "github.com/alex/homelab";

pub struct World {
    pub pool: PgPool,
    pub state: AppState,
    pub tei: Arc<FakeTei>,
    pub alice: Seeded,
}

pub async fn world(pool: PgPool) -> World {
    let (state, tei) = test_state_with(pool.clone(), test_settings(), AppliersRegistry::default());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    World { pool, state, tei, alice }
}

pub fn project() -> Scope {
    Scope::new(ScopeKind::Project, PROJECT)
}

/// One `embeddings` part for a row, from the FakeTei vector of `text`.
pub async fn embed(w: &World, space: Uuid, table: &str, id: Uuid, part: &str, scope: &Scope, text: &str) {
    sqlx::query(
        "INSERT INTO embeddings (space_id, owner_table, owner_id, part, model, embedding, scope_kind, scope_key)
         VALUES ($1, $2, $3, $4, 'bge-m3', $5, $6, $7)",
    )
    .bind(space)
    .bind(table)
    .bind(id)
    .bind(part)
    .bind(Vector::from(w.tei.vector(text)))
    .bind(scope.kind)
    .bind(&scope.key)
    .execute(&w.pool)
    .await
    .expect("insert embedding");
}

#[derive(Debug, Clone)]
pub struct Obs {
    pub space: Uuid,
    pub scope: Scope,
    pub obs_type: &'static str,
    pub title: String,
    pub narrative: String,
    pub importance: f32,
    pub sensitivity: Sensitivity,
    pub age_days: f64,
    pub files: Vec<String>,
    pub entities: Vec<String>,
    pub session: Option<Uuid>,
    pub source_event_ids: Vec<Uuid>,
    pub embed: bool,
}

impl Obs {
    pub fn new(space: Uuid, title: &str, narrative: &str) -> Self {
        Self {
            space,
            scope: project(),
            obs_type: "discovery",
            title: title.into(),
            narrative: narrative.into(),
            importance: 0.5,
            sensitivity: Sensitivity::Normal,
            age_days: 1.0,
            files: Vec::new(),
            entities: Vec::new(),
            session: None,
            source_event_ids: Vec::new(),
            embed: true,
        }
    }
}

pub async fn insert_obs(w: &World, o: Obs) -> Uuid {
    let id = new_id();
    sqlx::query(
        "INSERT INTO observations (id, space_id, scope_kind, scope_key, session_id, type, title, narrative, files,
                                   entities, importance, sensitivity, source_event_ids, extracted_by, content_hash,
                                   token_estimate, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                 now() - make_interval(secs => $17))",
    )
    .bind(id)
    .bind(o.space)
    .bind(o.scope.kind)
    .bind(&o.scope.key)
    .bind(o.session)
    .bind(o.obs_type)
    .bind(&o.title)
    .bind(&o.narrative)
    .bind(&o.files)
    .bind(&o.entities)
    .bind(o.importance)
    .bind(o.sensitivity)
    .bind(&o.source_event_ids)
    .bind(json!({"runner_kind": "claude-p", "model": "sonnet", "prompt": "observer.code@v1"}))
    .bind(id.to_string())
    .bind(memvault_core::estimate_tokens(&format!("{} {}", o.title, o.narrative)))
    .bind(o.age_days * 86400.0)
    .execute(&w.pool)
    .await
    .expect("insert observation");
    if o.embed {
        embed(w, o.space, "observations", id, "title", &o.scope, &o.title).await;
        embed(w, o.space, "observations", id, "narrative", &o.scope, &o.narrative).await;
    }
    id
}

pub async fn insert_session(pool: &PgPool, space: Uuid, client: Uuid, project: &str, minutes_ago: f64) -> Uuid {
    let id = new_id();
    sqlx::query(
        "INSERT INTO sessions (id, space_id, client_id, harness, external_session_id, project_key, started_at)
         VALUES ($1, $2, $3, 'claude-code', $4, $5, now() - make_interval(secs => $6))",
    )
    .bind(id)
    .bind(space)
    .bind(client)
    .bind(id.to_string())
    .bind(project)
    .bind(minutes_ago * 60.0)
    .execute(pool)
    .await
    .expect("insert session");
    id
}

pub async fn insert_event(pool: &PgPool, space: Uuid, session: Uuid, seq: i32) -> Uuid {
    let id = new_id();
    sqlx::query(
        "INSERT INTO events (id, space_id, session_id, seq, kind, payload, idempotency_key, occurred_at)
         VALUES ($1, $2, $3, $4, 'tool_use', '{}', $5, now())",
    )
    .bind(id)
    .bind(space)
    .bind(session)
    .bind(seq)
    .bind(id.to_string())
    .execute(pool)
    .await
    .expect("insert event");
    id
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_summary(
    w: &World,
    space: Uuid,
    scope: &Scope,
    session: Option<Uuid>,
    grain: SummaryGrain,
    request: &str,
    next_steps: &str,
    age_days: f64,
) -> Uuid {
    let id = new_id();
    sqlx::query(
        "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, session_id, request, next_steps,
                                extracted_by, token_estimate, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, '{}', $9, now() - make_interval(secs => $10))",
    )
    .bind(id)
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(grain)
    .bind(session)
    .bind(request)
    .bind(next_steps)
    .bind(memvault_core::estimate_tokens(&format!("{request} {next_steps}")))
    .bind(age_days * 86400.0)
    .execute(&w.pool)
    .await
    .expect("insert summary");
    embed(w, space, "summaries", id, "request", scope, request).await;
    id
}

#[derive(Debug, Clone)]
pub struct Fact {
    pub space: Uuid,
    pub scope: Scope,
    pub predicate: String,
    pub value_text: String,
    pub confidence: f32,
    pub pinned: bool,
    pub sensitivity: Sensitivity,
    pub evidence: Vec<Uuid>,
    pub client: Option<Uuid>,
}

impl Fact {
    pub fn new(space: Uuid, predicate: &str, value_text: &str) -> Self {
        Self {
            space,
            scope: Scope::global(),
            predicate: predicate.into(),
            value_text: value_text.into(),
            confidence: 0.8,
            pinned: false,
            sensitivity: Sensitivity::Normal,
            evidence: Vec::new(),
            client: None,
        }
    }
}

pub async fn insert_fact(w: &World, f: Fact) -> Uuid {
    let subject: Uuid = sqlx::query_scalar(
        "INSERT INTO entities (id, space_id, kind, canonical_name) VALUES ($1, $2, 'user', 'alex')
         ON CONFLICT (space_id, kind, canonical_name) DO UPDATE SET description = entities.description
         RETURNING id",
    )
    .bind(new_id())
    .bind(f.space)
    .fetch_one(&w.pool)
    .await
    .expect("insert entity");
    let id = new_id();
    sqlx::query(
        "INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text, confidence,
                            status, pinned, sensitivity, evidence_observation_ids, written_by_client_id, extracted_by)
         VALUES ($1, $2, $3, $4, $5, $6, to_jsonb($7::text), $7, $8, 'active', $9, $10, $11, $12,
                 '{\"model\": \"sonnet\"}')",
    )
    .bind(id)
    .bind(f.space)
    .bind(f.scope.kind)
    .bind(&f.scope.key)
    .bind(subject)
    .bind(&f.predicate)
    .bind(&f.value_text)
    .bind(f.confidence)
    .bind(f.pinned)
    .bind(f.sensitivity)
    .bind(&f.evidence)
    .bind(f.client)
    .execute(&w.pool)
    .await
    .expect("insert fact");
    let text = format!("{} {}", f.predicate.replace('.', " "), f.value_text);
    embed(w, f.space, "facts", id, "value", &f.scope, &text).await;
    id
}

pub async fn insert_procedure(w: &World, space: Uuid, name: &str, task_class: &str, body: &str) -> Uuid {
    let id = new_id();
    let scope = project();
    sqlx::query(
        "INSERT INTO procedures (id, space_id, scope_kind, scope_key, name, task_class, trigger_text, body_md,
                                 token_estimate)
         VALUES ($1, $2, $3, $4, $5, $6, $5, $7, $8)",
    )
    .bind(id)
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(name)
    .bind(task_class)
    .bind(body)
    .bind(memvault_core::estimate_tokens(body))
    .execute(&w.pool)
    .await
    .expect("insert procedure");
    embed(w, space, "procedures", id, "body", &scope, &format!("{name} {body}")).await;
    id
}

/// One live chunk of the source `uri`, embedded from its heading path and content.
pub async fn insert_document(
    w: &World,
    space: Uuid,
    scope: &Scope,
    uri: &str,
    headings: &[&str],
    content: &str,
) -> Uuid {
    let id = new_id();
    let headings: Vec<String> = headings.iter().map(|h| h.to_string()).collect();
    sqlx::query(
        "INSERT INTO documents (id, space_id, scope_kind, scope_key, source_uri, title, content_type, chunk_index,
                                heading_path, content, content_hash, token_estimate, ingested_by)
         VALUES ($1, $2, $3, $4, $5, $5, 'markdown', 0, $6, $7, 'h1', $8, $9)",
    )
    .bind(id)
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(uri)
    .bind(&headings)
    .bind(content)
    .bind(memvault_core::estimate_tokens(content))
    .bind(w.alice.client_id)
    .execute(&w.pool)
    .await
    .expect("insert document");
    embed(w, space, "documents", id, "body:0", scope, &format!("{} {content}", headings.join(" "))).await;
    id
}

pub async fn insert_work_state(pool: &PgPool, space: Uuid, scope: &Scope, list: &str, fields: serde_json::Value) {
    sqlx::query(
        "INSERT INTO work_state (id, space_id, scope_kind, scope_key, list_name, fields) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(new_id())
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(list)
    .bind(fields)
    .execute(pool)
    .await
    .expect("insert work_state");
}

/// A fact chain: `vim` held from `2026-01-01` (no `valid_from`, so `first_seen_at` counts) until it was
/// superseded on `2026-03-01` by `helix`; `dup` is a merged duplicate candidate that never held.
pub struct EditorHistory {
    pub vim: Uuid,
    pub helix: Uuid,
    pub dup: Uuid,
}

pub async fn insert_editor_history(w: &World) -> EditorHistory {
    let space = w.alice.space_id;
    let vim = insert_fact(w, Fact::new(space, "prefers.editor", "vim")).await;
    sqlx::query(
        "UPDATE facts SET status = 'superseded', first_seen_at = '2026-01-01T00:00:00Z',
                          valid_to = '2026-03-01T00:00:00Z' WHERE id = $1",
    )
    .bind(vim)
    .execute(&w.pool)
    .await
    .expect("end the first fact");
    let dup = insert_fact(w, Fact::new(space, "prefers.editor", "vim again")).await;
    sqlx::query(
        "UPDATE facts SET status = 'superseded', superseded_by = $2, first_seen_at = '2026-01-15T00:00:00Z'
          WHERE id = $1",
    )
    .bind(dup)
    .bind(vim)
    .execute(&w.pool)
    .await
    .expect("merge the duplicate");
    let helix = insert_fact(w, Fact::new(space, "prefers.editor", "helix")).await;
    sqlx::query("UPDATE facts SET valid_from = '2026-03-01T00:00:00Z', supersedes_id = $2 WHERE id = $1")
        .bind(helix)
        .bind(vim)
        .execute(&w.pool)
        .await
        .expect("link the new fact");
    sqlx::query("UPDATE facts SET superseded_by = $2 WHERE id = $1")
        .bind(vim)
        .bind(helix)
        .execute(&w.pool)
        .await
        .expect("link the old fact");
    sqlx::query(
        "INSERT INTO memory_audit (id, space_id, actor, action, target_table, target_id, reason, model, run_id,
                                   created_at)
         VALUES ($1, $2, 'planner', 'fact.supersede', 'facts', $3, 'switched in March', 'sonnet', $4,
                 '2026-03-01T00:00:00Z')",
    )
    .bind(new_id())
    .bind(space)
    .bind(vim)
    .bind(new_id())
    .execute(&w.pool)
    .await
    .expect("audit the supersession");
    EditorHistory { vim, helix, dup }
}
