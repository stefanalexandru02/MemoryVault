//! Explicit writes: `POST /v1/memories` (remember) and `DELETE /v1/memories/{id}` (forget), spec 6.4.
//!
//! Facts land `active` at confidence 0.95 with `written_by_client_id`; observations get
//! `extracted_by = {"runner_kind": "explicit"}`; procedures are versioned. Every write is audited,
//! queues an embed job and signals the context cache, all in the request's transaction. `remember`
//! and `forget` are shared with the MCP tools.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;
use memvault_core::auth::{ADMIN, MEMORIES_WRITE, resolve_write_space, writable_space_ids};
use memvault_core::http::{Json, Path, Query};
use memvault_core::ids::new_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::jobs::enqueue_embed;
use memvault_core::schemas::common::{MemoryTable, Scope, ScopeKind, estimate_tokens};
use memvault_core::schemas::memories::{
    FactSubject, ForgetAction, ForgetQuery, ForgetResponse, RememberFact, RememberObservation, RememberProcedure,
    RememberRequest, RememberResponse,
};
use memvault_core::{ApiError, AppState, Identity, require_scope};

use memvault_core::user_tx;
use memvault_recall::engine::validate_scopes;
use memvault_recall::render;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

/// Confidence of a fact the user stated explicitly.
const EXPLICIT_CONFIDENCE: f32 = 0.95;

pub(crate) async fn create(
    axum::extract::State(state): axum::extract::State<AppState>,
    identity: Identity,
    Json(body): Json<RememberRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_WRITE).await?;
    Ok((StatusCode::CREATED, Json(remember(&state, &identity, body).await?)))
}

pub(crate) async fn delete(
    axum::extract::State(state): axum::extract::State<AppState>,
    identity: Identity,
    Path(id): Path<Uuid>,
    Query(query): Query<ForgetQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_WRITE).await?;
    query.validate()?;
    if query.hard {
        identity.ensure(ADMIN)?;
    }
    let reason = query.reason.as_deref().map(str::trim).filter(|r| !r.is_empty());
    Ok(Json(forget(&state, &identity, id, query.hard, reason).await?))
}

// ---------- shared helpers ----------

/// The caller's current project (`clients.current_project_key`), kept up to date by the event path.
pub(crate) async fn current_project(
    conn: &mut PgConnection,
    identity: &Identity,
) -> Result<Option<String>, sqlx::Error> {
    let key: Option<Option<String>> = sqlx::query_scalar("SELECT current_project_key FROM clients WHERE id = $1")
        .bind(identity.client_id)
        .fetch_optional(conn)
        .await?;
    Ok(key.flatten().filter(|k| !k.is_empty()))
}

/// The requested scope, validated, or the current project's scope, else global.
pub(crate) async fn write_scope(
    conn: &mut PgConnection,
    identity: &Identity,
    requested: Option<&Scope>,
) -> Result<Scope, ApiError> {
    if let Some(scope) = requested {
        return Ok(validate_scopes(std::slice::from_ref(scope))?.remove(0));
    }
    Ok(match current_project(conn, identity).await? {
        Some(key) => Scope::new(ScopeKind::Project, key),
        None => Scope::global(),
    })
}

struct Audit<'a> {
    space_id: Uuid,
    action: &'a str,
    table: MemoryTable,
    target_id: Uuid,
    before: Option<Value>,
    after: Option<Value>,
    reason: Option<&'a str>,
}

async fn record_audit(conn: &mut PgConnection, identity: &Identity, a: Audit<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO memory_audit (id, space_id, actor, action, target_table, target_id, before, after, reason)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(new_id())
    .bind(a.space_id)
    .bind(identity.actor())
    .bind(a.action)
    .bind(a.table)
    .bind(a.target_id)
    .bind(a.before)
    .bind(a.after)
    .bind(a.reason)
    .execute(conn)
    .await?;
    Ok(())
}

/// The row as JSON without its generated columns.
async fn snapshot(conn: &mut PgConnection, table: MemoryTable, id: Uuid) -> Result<Option<Value>, sqlx::Error> {
    sqlx::query_scalar(&format!("SELECT to_jsonb(t) - 'search_text' - 'search_tsv' FROM {table} t WHERE id = $1"))
        .bind(id)
        .fetch_optional(conn)
        .await
}

// ---------- remember ----------

/// Write one fact, observation or procedure for the caller (spec 6.4).
pub async fn remember(
    state: &AppState,
    identity: &Identity,
    request: RememberRequest,
) -> Result<RememberResponse, ApiError> {
    request.validate()?;
    let mut tx = user_tx(&state.db.app, identity).await?;
    let space = resolve_write_space(&mut tx, identity, request.space_id()).await?;
    let scope = write_scope(&mut tx, identity, request.scope()).await?;
    let out = match &request {
        RememberRequest::Fact(f) => remember_fact(&mut tx, identity, space, &scope, f).await?,
        RememberRequest::Observation(o) => remember_observation(&mut tx, identity, space, &scope, o).await?,
        RememberRequest::Procedure(p) => remember_procedure(&mut tx, identity, space, &scope, p).await?,
    };
    notify_invalidate(&mut tx, space, scope.kind, &scope.key).await?;
    tx.commit().await?;
    Ok(out)
}

/// An entity of the space by canonical name or alias, created when missing.
async fn resolve_entity(conn: &mut PgConnection, space: Uuid, subject: &FactSubject) -> Result<Uuid, sqlx::Error> {
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM entities WHERE space_id = $1 AND kind = $2 AND (canonical_name = $3 OR $3 = ANY(aliases))
          ORDER BY canonical_name = $3 DESC LIMIT 1",
    )
    .bind(space)
    .bind(&subject.kind)
    .bind(&subject.name)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = found {
        return Ok(id);
    }
    sqlx::query_scalar(
        "INSERT INTO entities (id, space_id, kind, canonical_name) VALUES ($1, $2, $3, $4)
         ON CONFLICT (space_id, kind, canonical_name) DO UPDATE SET canonical_name = entities.canonical_name
         RETURNING id",
    )
    .bind(new_id())
    .bind(space)
    .bind(&subject.kind)
    .bind(&subject.name)
    .fetch_one(conn)
    .await
}

async fn remember_fact(
    conn: &mut PgConnection,
    identity: &Identity,
    space: Uuid,
    scope: &Scope,
    f: &RememberFact,
) -> Result<RememberResponse, ApiError> {
    // FOR UPDATE locks nothing when no active fact exists yet, so concurrent first writes of the same
    // key are serialised by a transaction-scoped advisory lock instead. It is taken before the entity
    // upsert (which would otherwise deadlock with it) and is keyed without the subject, which may be an alias.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("fact:{space}:{}:{}:{}", scope.kind, scope.key, f.predicate))
        .execute(&mut *conn)
        .await?;
    let subject_id = resolve_entity(conn, space, &f.subject).await?;
    let value = if f.value.is_null() { Value::String(f.value_text.clone()) } else { f.value.clone() };
    let active: Option<(Uuid, String, Value)> = sqlx::query_as(
        "SELECT id, value_text, value FROM facts
          WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND subject_id = $4 AND predicate = $5
            AND status = 'active' FOR UPDATE",
    )
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(subject_id)
    .bind(&f.predicate)
    .fetch_optional(&mut *conn)
    .await?;

    // Stating what is already known again confirms the active fact.
    if let Some((id, text, old_value)) = &active
        && *text == f.value_text
        && *old_value == value
    {
        let before = snapshot(conn, MemoryTable::Facts, *id).await?;
        sqlx::query(
            "UPDATE facts SET confirmations = confirmations + 1, last_confirmed_at = now(),
                              confidence = GREATEST(confidence, $2), pinned = pinned OR $3,
                              learned_on_client_ids = (SELECT coalesce(array_agg(DISTINCT x), '{}')
                                                         FROM unnest(learned_on_client_ids || $4::uuid) x)
              WHERE id = $1",
        )
        .bind(id)
        .bind(EXPLICIT_CONFIDENCE)
        .bind(f.pinned)
        .bind(identity.client_id)
        .execute(&mut *conn)
        .await?;
        let after = snapshot(conn, MemoryTable::Facts, *id).await?;
        let audit = Audit {
            space_id: space,
            action: "fact.confirm",
            table: MemoryTable::Facts,
            target_id: *id,
            before,
            after,
            reason: Some("stated again explicitly"),
        };
        record_audit(conn, identity, audit).await?;
        return Ok(fact_response(*id, f));
    }

    // A different value replaces the active fact: the new one starts as a candidate so that the
    // one-active-fact index holds while the old one is superseded.
    let id = new_id();
    sqlx::query(
        "INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text, confidence,
                            status, pinned, sensitivity, written_by_client_id, learned_on_client_ids, extracted_by,
                            valid_from, supersedes_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, ARRAY[$13]::uuid[],
                 '{\"runner_kind\": \"explicit\"}'::jsonb, now(), $14)",
    )
    .bind(id)
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(subject_id)
    .bind(&f.predicate)
    .bind(&value)
    .bind(&f.value_text)
    .bind(EXPLICIT_CONFIDENCE)
    .bind(if active.is_some() { "candidate" } else { "active" })
    .bind(f.pinned)
    .bind(f.sensitivity)
    .bind(identity.client_id)
    .bind(active.as_ref().map(|a| a.0))
    .execute(&mut *conn)
    .await?;
    if let Some((old_id, _, _)) = &active {
        let before_old = snapshot(conn, MemoryTable::Facts, *old_id).await?;
        sqlx::query("UPDATE facts SET status = 'superseded', valid_to = now(), superseded_by = $2 WHERE id = $1")
            .bind(old_id)
            .bind(id)
            .execute(&mut *conn)
            .await?;
        sqlx::query("UPDATE facts SET status = 'active' WHERE id = $1").bind(id).execute(&mut *conn).await?;
        let audit = Audit {
            space_id: space,
            action: "fact.supersede",
            table: MemoryTable::Facts,
            target_id: *old_id,
            before: before_old,
            after: snapshot(conn, MemoryTable::Facts, *old_id).await?,
            reason: Some("replaced by an explicit write"),
        };
        record_audit(conn, identity, audit).await?;
    }
    let audit = Audit {
        space_id: space,
        action: "fact.create",
        table: MemoryTable::Facts,
        target_id: id,
        before: None,
        after: snapshot(conn, MemoryTable::Facts, id).await?,
        reason: Some("remembered explicitly"),
    };
    record_audit(conn, identity, audit).await?;
    enqueue_embed(conn, MemoryTable::Facts.as_str(), id).await?;
    Ok(fact_response(id, f))
}

fn fact_response(id: Uuid, f: &RememberFact) -> RememberResponse {
    let subject = if f.subject.kind == "user" { String::new() } else { format!("{} ", f.subject.name) };
    let tag = render::fact_tag(&id, f64::from(EXPLICIT_CONFIDENCE), &[], Some(Utc::now()));
    RememberResponse {
        id,
        table: MemoryTable::Facts,
        status: "active".into(),
        rendered: format!("- {subject}{}: {} {tag}", f.predicate, render::one_line(&f.value_text)),
    }
}

async fn remember_observation(
    conn: &mut PgConnection,
    identity: &Identity,
    space: Uuid,
    scope: &Scope,
    o: &RememberObservation,
) -> Result<RememberResponse, ApiError> {
    let id = new_id();
    let mut hashed = vec![o.obs_type.as_str(), o.title.as_str()];
    hashed.extend(o.facts.iter().map(String::as_str));
    let content_hash = hex::encode(Sha256::digest(hashed.join("|").as_bytes()));
    let mut body = vec![o.title.as_str(), o.subtitle.as_deref().unwrap_or(""), o.narrative.as_deref().unwrap_or("")];
    body.extend(o.facts.iter().map(String::as_str));
    let tokens = estimate_tokens(&body.join(" "));
    sqlx::query(
        "INSERT INTO observations (id, space_id, scope_kind, scope_key, type, title, subtitle, narrative, facts,
                                   concepts, files, importance, sensitivity, extracted_by, written_by_client_id,
                                   content_hash, token_estimate)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 '{\"runner_kind\": \"explicit\"}'::jsonb, $14, $15, $16)",
    )
    .bind(id)
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(o.obs_type)
    .bind(&o.title)
    .bind(&o.subtitle)
    .bind(&o.narrative)
    .bind(&o.facts)
    .bind(&o.concepts)
    .bind(&o.files)
    .bind(o.importance as f32)
    .bind(o.sensitivity)
    .bind(identity.client_id)
    .bind(content_hash)
    .bind(tokens)
    .execute(&mut *conn)
    .await?;
    let audit = Audit {
        space_id: space,
        action: "obs.create",
        table: MemoryTable::Observations,
        target_id: id,
        before: None,
        after: Some(json!({"type": o.obs_type, "title": o.title})),
        reason: Some("remembered explicitly"),
    };
    record_audit(conn, identity, audit).await?;
    enqueue_embed(conn, MemoryTable::Observations.as_str(), id).await?;
    let entry = render::IndexEntry {
        id,
        when: Utc::now(),
        glyph: render::glyph(MemoryTable::Observations, Some(o.obs_type.as_str())),
        title: o.title.clone(),
        tokens,
        anchor: false,
    };
    Ok(RememberResponse {
        id,
        table: MemoryTable::Observations,
        status: "created".into(),
        rendered: render::index_line(&entry, false),
    })
}

async fn remember_procedure(
    conn: &mut PgConnection,
    identity: &Identity,
    space: Uuid,
    scope: &Scope,
    p: &RememberProcedure,
) -> Result<RememberResponse, ApiError> {
    // A procedure of the same name in the same scope is revised: the old version is retired.
    let previous: Option<(Uuid, i32)> = sqlx::query_as(
        "SELECT id, version FROM procedures
          WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND name = $4 AND status = 'active'
          ORDER BY version DESC LIMIT 1 FOR UPDATE",
    )
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(&p.name)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((old_id, _)) = &previous {
        let before = snapshot(conn, MemoryTable::Procedures, *old_id).await?;
        sqlx::query("UPDATE procedures SET status = 'retired' WHERE id = $1").bind(old_id).execute(&mut *conn).await?;
        let audit = Audit {
            space_id: space,
            action: "proc.retire",
            table: MemoryTable::Procedures,
            target_id: *old_id,
            before,
            after: snapshot(conn, MemoryTable::Procedures, *old_id).await?,
            reason: Some("revised by a newer version"),
        };
        record_audit(conn, identity, audit).await?;
    }
    let id = new_id();
    let version = previous.map_or(1, |(_, v)| v + 1);
    sqlx::query(
        "INSERT INTO procedures (id, space_id, scope_kind, scope_key, name, task_class, trigger_text, body_md, version,
                                 supersedes_id, token_estimate)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(&p.name)
    .bind(&p.task_class)
    .bind(&p.trigger_text)
    .bind(&p.body_md)
    .bind(version)
    .bind(previous.map(|(old_id, _)| old_id))
    .bind(estimate_tokens(&p.body_md))
    .execute(&mut *conn)
    .await?;
    let audit = Audit {
        space_id: space,
        action: if previous.is_some() { "proc.revise" } else { "proc.create" },
        table: MemoryTable::Procedures,
        target_id: id,
        before: None,
        after: snapshot(conn, MemoryTable::Procedures, id).await?,
        reason: Some("remembered explicitly"),
    };
    record_audit(conn, identity, audit).await?;
    enqueue_embed(conn, MemoryTable::Procedures.as_str(), id).await?;
    let tag = render::procedure_tag(&id, version, Some(Utc::now()));
    Ok(RememberResponse {
        id,
        table: MemoryTable::Procedures,
        status: "active".into(),
        rendered: format!(
            "- **{}** (v{version}): {} {tag}",
            render::one_line(&p.name),
            render::excerpt(&p.body_md, 160)
        ),
    })
}

// ---------- forget ----------

/// Soft delete (fact `retracted`, observation `archived_at`, procedure `retired`), or with `hard`
/// (the caller has checked `admin`) remove the row, its embeddings, references and audit content.
/// Both are audited and signal the context cache. Forgetting twice is a no-op.
pub async fn forget(
    state: &AppState,
    identity: &Identity,
    id: Uuid,
    hard: bool,
    reason: Option<&str>,
) -> Result<ForgetResponse, ApiError> {
    let mut tx = user_tx(&state.db.app, identity).await?;
    let (table, action) = forget_in(&mut tx, identity, id, hard, reason).await?;
    tx.commit().await?;
    Ok(ForgetResponse { id, table, action })
}

async fn forget_in(
    conn: &mut PgConnection,
    identity: &Identity,
    id: Uuid,
    hard: bool,
    reason: Option<&str>,
) -> Result<(MemoryTable, ForgetAction), ApiError> {
    let found: Option<(MemoryTable, Uuid, ScopeKind, String)> = sqlx::query_as(
        "SELECT 'facts', space_id, scope_kind, scope_key FROM facts WHERE id = $1
         UNION ALL SELECT 'observations', space_id, scope_kind, scope_key FROM observations WHERE id = $1
         UNION ALL SELECT 'procedures', space_id, scope_kind, scope_key FROM procedures WHERE id = $1
         UNION ALL SELECT 'summaries', space_id, scope_kind, scope_key FROM summaries WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let (table, space, kind, key) = found.ok_or_else(|| ApiError::not_found("memory not found"))?;
    let writable = writable_space_ids(conn, identity.user_id).await?;
    if !writable.contains(&space) {
        return Err(ApiError::forbidden("space_forbidden", format!("no write access to space {space}")));
    }
    let scope = Scope::new(kind, key);
    let action = if hard {
        hard_delete(conn, identity, table, space, id, &scope, reason).await?
    } else {
        soft_delete(conn, identity, table, space, id, reason).await?
    };
    if table == MemoryTable::Facts {
        // The scope's cached profile may render this fact. Drop it so context and recall fall back to
        // the live facts until the planner re-renders it.
        sqlx::query("DELETE FROM profiles WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3")
            .bind(space)
            .bind(scope.kind)
            .bind(&scope.key)
            .execute(&mut *conn)
            .await?;
    }
    notify_invalidate(conn, space, scope.kind, &scope.key).await?;
    Ok((table, action))
}

async fn soft_delete(
    conn: &mut PgConnection,
    identity: &Identity,
    table: MemoryTable,
    space: Uuid,
    id: Uuid,
    reason: Option<&str>,
) -> Result<ForgetAction, ApiError> {
    let (update, audit_action, action) = match table {
        MemoryTable::Facts => (
            "UPDATE facts SET status = 'retracted', valid_to = coalesce(valid_to, now()), conflict = false
              WHERE id = $1 AND status <> 'retracted'",
            "fact.retract",
            ForgetAction::Retracted,
        ),
        MemoryTable::Observations => (
            "UPDATE observations SET archived_at = now() WHERE id = $1 AND archived_at IS NULL",
            "obs.archive",
            ForgetAction::Archived,
        ),
        MemoryTable::Procedures => (
            "UPDATE procedures SET status = 'retired' WHERE id = $1 AND status <> 'retired'",
            "proc.retire",
            ForgetAction::Retired,
        ),
        MemoryTable::Summaries => {
            return Err(ApiError::invalid("summaries cannot be forgotten; forget the observations they rest on"));
        }
    };
    let before = snapshot(conn, table, id).await?;
    if sqlx::query(update).bind(id).execute(&mut *conn).await?.rows_affected() > 0 {
        let after = snapshot(conn, table, id).await?;
        let audit = Audit {
            space_id: space,
            action: audit_action,
            table,
            target_id: id,
            before,
            after,
            reason: Some(reason.unwrap_or("forgotten explicitly")),
        };
        record_audit(conn, identity, audit).await?;
    }
    Ok(action)
}

async fn hard_delete(
    conn: &mut PgConnection,
    identity: &Identity,
    table: MemoryTable,
    space: Uuid,
    id: Uuid,
    scope: &Scope,
    reason: Option<&str>,
) -> Result<ForgetAction, ApiError> {
    // Drop what points at the row, so the delete cannot fail on a reference and nothing keeps its content.
    let cleanup: &[&str] = match table {
        MemoryTable::Observations => &[
            "UPDATE observations SET superseded_by = NULL WHERE superseded_by = $1",
            "UPDATE procedure_hints SET observation_id = NULL WHERE observation_id = $1",
            "UPDATE facts SET evidence_observation_ids = array_remove(evidence_observation_ids, $1)
              WHERE $1 = ANY(evidence_observation_ids)",
            "UPDATE procedures SET source_observation_ids = array_remove(source_observation_ids, $1)
              WHERE $1 = ANY(source_observation_ids)",
        ],
        MemoryTable::Facts => &[
            "UPDATE facts SET supersedes_id = NULL WHERE supersedes_id = $1",
            "UPDATE facts SET superseded_by = NULL WHERE superseded_by = $1",
        ],
        MemoryTable::Procedures => &["UPDATE procedures SET supersedes_id = NULL WHERE supersedes_id = $1"],
        MemoryTable::Summaries => &[],
    };
    for sql in cleanup {
        sqlx::query(sql).bind(id).execute(&mut *conn).await?;
    }
    sqlx::query("DELETE FROM embeddings WHERE owner_table = $1 AND owner_id = $2")
        .bind(table)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    // Work packets (consolidation inputs and results) that carry the row's id keep its content for
    // their retention period: blank them, and cancel the ones still waiting to run.
    sqlx::query(
        "UPDATE work_packets
            SET inputs = '{}'::jsonb, result = NULL, applied = NULL,
                status = CASE WHEN status IN ('pending', 'leased') THEN 'cancelled' ELSE status END
          WHERE space_id = $1
            AND (strpos(inputs::text, $2) > 0 OR strpos(coalesce(result::text, ''), $2) > 0
                 OR strpos(coalesce(applied::text, ''), $2) > 0)",
    )
    .bind(space)
    .bind(id.to_string())
    .execute(&mut *conn)
    .await?;
    // Earlier audit rows keep who and when, not the content that is being removed.
    sqlx::query("UPDATE memory_audit SET before = NULL, after = NULL WHERE target_table = $1 AND target_id = $2")
        .bind(table)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    let deleted = sqlx::query(&format!("DELETE FROM {table} WHERE id = $1")).bind(id).execute(&mut *conn).await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError::forbidden("forbidden", "the memory could not be deleted"));
    }
    let audit = Audit {
        space_id: space,
        action: "delete.hard",
        table,
        target_id: id,
        before: Some(json!({"scope": scope.as_text()})),
        after: None,
        reason: Some(reason.unwrap_or("hard delete")),
    };
    record_audit(conn, identity, audit).await?;
    Ok(ForgetAction::Deleted)
}
