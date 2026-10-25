//! Admin operations (spec 6.7, 5.2) shared by the REST routes and the admin CLI. Every function runs
//! on a `system_tx` connection: users, spaces, clients, tokens and jobs have no RLS, and the
//! tenancy and remap writes are admin-only by construction.

use std::collections::BTreeMap;

use chrono::TimeDelta;
use memvault_core::ApiError;
use memvault_core::auth::admin as tenancy;
use memvault_core::auth::{self, MintOptions};
use memvault_core::ids::new_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::admin::{
    MembershipCreate, MembershipOut, PlannerRunOut, ReindexRequest, ReindexResponse, SpaceCreate, SpaceOut,
    TokenCreate, TokenMinted, TokenOut, UserCreate, UserOut,
};
use memvault_core::schemas::common::{EmbeddingOwnerTable, PacketKind, PacketStatus, PlanTrigger, ScopeKind};
use memvault_core::schemas::work::PacketRecord;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{AuditCtx, record, snapshot};
use crate::profiles;

const USER_COLUMNS: &str = "id, handle, display_name, default_space_id, created_at, disabled_at";
const REINDEX_CHUNK: usize = 1000;
/// `jobs.priority` of backfill embed jobs; live ones have 0 and are claimed first (migration 0500).
const REINDEX_PRIORITY: i16 = 10;

// ---------- users, spaces, memberships ----------

pub async fn create_user(conn: &mut PgConnection, body: &UserCreate) -> Result<UserOut, ApiError> {
    body.validate()?;
    let created = tenancy::create_user(conn, &body.handle, body.display_name.as_deref()).await?;
    Ok(sqlx::query_as(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1"))
        .bind(created.user_id)
        .fetch_one(conn)
        .await?)
}

pub async fn list_users(conn: &mut PgConnection) -> Result<Vec<UserOut>, ApiError> {
    Ok(sqlx::query_as(&format!("SELECT {USER_COLUMNS} FROM users ORDER BY created_at, id")).fetch_all(conn).await?)
}

/// The user with `user_id` or `handle` (404 when unknown).
pub async fn resolve_user(
    conn: &mut PgConnection,
    user_id: Option<Uuid>,
    handle: Option<&str>,
) -> Result<Uuid, ApiError> {
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1 OR handle = $2 LIMIT 1")
        .bind(user_id)
        .bind(handle)
        .fetch_optional(conn)
        .await?;
    id.ok_or_else(|| ApiError::not_found("unknown user"))
}

pub async fn create_space(conn: &mut PgConnection, body: &SpaceCreate) -> Result<SpaceOut, ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::invalid("name must not be empty"));
    }
    resolve_user(conn, Some(body.owner_id), None).await?;
    let id = tenancy::create_space(conn, &body.name, body.kind, body.owner_id).await?;
    Ok(sqlx::query_as("SELECT id, name, kind, owner_id, created_at FROM spaces WHERE id = $1")
        .bind(id)
        .fetch_one(conn)
        .await?)
}

pub async fn list_spaces(conn: &mut PgConnection) -> Result<Vec<SpaceOut>, ApiError> {
    Ok(sqlx::query_as("SELECT id, name, kind, owner_id, created_at FROM spaces ORDER BY created_at, id")
        .fetch_all(conn)
        .await?)
}

/// A space by id, or by exact name when `reference` is not a UUID (409 when the name is ambiguous).
pub async fn resolve_space(conn: &mut PgConnection, reference: &str) -> Result<Uuid, ApiError> {
    let ids: Vec<Uuid> = match reference.parse::<Uuid>() {
        Ok(id) => sqlx::query_scalar("SELECT id FROM spaces WHERE id = $1").bind(id).fetch_all(conn).await?,
        Err(_) => sqlx::query_scalar("SELECT id FROM spaces WHERE name = $1").bind(reference).fetch_all(conn).await?,
    };
    match ids.as_slice() {
        [id] => Ok(*id),
        [] => Err(ApiError::not_found(format!("unknown space {reference:?}"))),
        _ => Err(ApiError::conflict(format!("several spaces are named {reference:?}; use the id"))),
    }
}

/// Insert or update a membership.
pub async fn add_membership(conn: &mut PgConnection, body: &MembershipCreate) -> Result<MembershipOut, ApiError> {
    resolve_space(conn, &body.space_id.to_string()).await?;
    resolve_user(conn, Some(body.user_id), None).await?;
    tenancy::add_membership(conn, body.space_id, body.user_id, body.role).await?;
    Ok(body.clone())
}

pub async fn list_memberships(conn: &mut PgConnection, space_id: Option<Uuid>) -> Result<Vec<MembershipOut>, ApiError> {
    Ok(sqlx::query_as(
        "SELECT space_id, user_id, role FROM memberships WHERE ($1::uuid IS NULL OR space_id = $1)
          ORDER BY space_id, user_id",
    )
    .bind(space_id)
    .fetch_all(conn)
    .await?)
}

// ---------- tokens ----------

/// Mint a token; the `(user, device_id)` client is created when missing. The raw token is in the
/// result and nowhere else.
pub async fn mint_token(conn: &mut PgConnection, pepper: &str, body: &TokenCreate) -> Result<TokenMinted, ApiError> {
    body.validate()?;
    let user_id = resolve_user(conn, body.user_id, body.handle.as_deref()).await?;
    let client_kind = body.client_kind.unwrap_or_else(|| body.kind.client_kind());
    let client_id = tenancy::ensure_client(
        conn,
        user_id,
        &body.device_id,
        client_kind,
        body.hostname.as_deref(),
        body.platform.as_deref(),
    )
    .await?;
    let options = MintOptions { expires_in: body.expires_days.map(TimeDelta::days), ..MintOptions::default() };
    let minted = auth::mint_token(conn, user_id, client_id, body.kind, pepper, options).await?;
    Ok(TokenMinted {
        token: minted.token,
        token_id: minted.token_id,
        prefix: minted.prefix,
        user_id,
        client_id,
        kind: minted.kind,
        scopes: minted.scopes,
        expires_at: minted.expires_at,
    })
}

pub async fn list_tokens(conn: &mut PgConnection, user_id: Option<Uuid>) -> Result<Vec<TokenOut>, ApiError> {
    Ok(sqlx::query_as(
        "SELECT id, user_id, client_id, kind, prefix, scopes, created_at, expires_at, revoked_at, last_used_at
           FROM api_tokens WHERE ($1::uuid IS NULL OR user_id = $1) ORDER BY created_at, id",
    )
    .bind(user_id)
    .fetch_all(conn)
    .await?)
}

/// Revoke by id or by the 8-character prefix. Revoking a revoked token succeeds; unknown is 404.
pub async fn revoke_token(conn: &mut PgConnection, reference: &str) -> Result<TokenOut, ApiError> {
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM api_tokens WHERE id::text = $1 OR prefix = $1")
        .bind(reference)
        .fetch_optional(&mut *conn)
        .await?;
    let id = id.ok_or_else(|| ApiError::not_found("unknown token"))?;
    auth::revoke_token(conn, id).await?;
    Ok(sqlx::query_as(
        "SELECT id, user_id, client_id, kind, prefix, scopes, created_at, expires_at, revoked_at, last_used_at
           FROM api_tokens WHERE id = $1",
    )
    .bind(id)
    .fetch_one(conn)
    .await?)
}

// ---------- jobs and packets ----------

pub fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(50).clamp(1, 500)
}

pub async fn list_jobs(
    conn: &mut PgConnection,
    status: Option<&str>,
    job_type: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<memvault_core::schemas::admin::JobOut>, ApiError> {
    Ok(sqlx::query_as(
        "SELECT id, type, payload, status, attempts, run_after, locked_by, locked_until, last_error, created_at,
                payload ->> 'trigger' AS trigger,
                CASE WHEN payload ? 'scope' THEN (payload #>> '{scope,kind}') || ':' || (payload #>> '{scope,key}') END AS scope
           FROM jobs WHERE ($1::text IS NULL OR status = $1) AND ($2::text IS NULL OR type = $2)
          ORDER BY created_at DESC, id DESC LIMIT $3",
    )
    .bind(status)
    .bind(job_type)
    .bind(clamp_limit(limit))
    .fetch_all(conn)
    .await?)
}

pub async fn list_runs(
    conn: &mut PgConnection,
    trigger: Option<PlanTrigger>,
    space_id: Option<Uuid>,
    limit: Option<i64>,
) -> Result<Vec<PlannerRunOut>, ApiError> {
    Ok(sqlx::query_as(
        "SELECT run_id, space_id, CASE WHEN scope_kind IS NOT NULL THEN scope_kind::text || ':' || scope_key END AS scope,
                trigger, packets, actions, created_at
           FROM planner_runs WHERE ($1::text IS NULL OR trigger = $1) AND ($2::uuid IS NULL OR space_id = $2)
          ORDER BY created_at DESC, run_id DESC LIMIT $3",
    )
    .bind(trigger)
    .bind(space_id)
    .bind(clamp_limit(limit))
    .fetch_all(conn)
    .await?)
}

pub async fn list_packets(
    conn: &mut PgConnection,
    status: Option<PacketStatus>,
    kind: Option<PacketKind>,
    space_id: Option<Uuid>,
    limit: Option<i64>,
) -> Result<Vec<PacketRecord>, ApiError> {
    Ok(sqlx::query_as(
        "SELECT * FROM work_packets
          WHERE ($1::text IS NULL OR status = $1) AND ($2::text IS NULL OR kind = $2)
            AND ($3::uuid IS NULL OR space_id = $3)
          ORDER BY created_at DESC, id DESC LIMIT $4",
    )
    .bind(status)
    .bind(kind)
    .bind(space_id)
    .bind(clamp_limit(limit))
    .fetch_all(conn)
    .await?)
}

/// Put a failed packet back in the queue (spec 9.3). With `reset_attempts` the attempts restart at
/// 0; otherwise `max_attempts` is raised so it gets one more try. 409 unless the packet is `failed`.
pub async fn retry_packet(conn: &mut PgConnection, id: Uuid, reset_attempts: bool) -> Result<PacketRecord, ApiError> {
    let status: Option<PacketStatus> = sqlx::query_scalar("SELECT status FROM work_packets WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    match status {
        None => return Err(ApiError::not_found("unknown packet")),
        Some(PacketStatus::Failed) => {}
        Some(other) => {
            return Err(ApiError::conflict(format!("only failed packets can be retried; this one is {other}")));
        }
    }
    Ok(sqlx::query_as(
        "UPDATE work_packets SET status = 'pending', leased_by = NULL, lease_until = NULL, available_after = now(),
                last_error = NULL, done_at = NULL,
                attempts = CASE WHEN $2 THEN 0 ELSE attempts END,
                max_attempts = CASE WHEN $2 THEN max_attempts ELSE greatest(max_attempts, attempts + 1) END
          WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(reset_attempts)
    .fetch_one(conn)
    .await?)
}

// ---------- reindex ----------

/// SQL condition selecting the rows of `table` that get embeddings (`true` when all do).
pub(crate) fn live_filter(table: EmbeddingOwnerTable) -> &'static str {
    match table {
        EmbeddingOwnerTable::Observations => "archived_at IS NULL",
        EmbeddingOwnerTable::Facts => "status IN ('candidate', 'active')",
        EmbeddingOwnerTable::Procedures => "status = 'active'",
        EmbeddingOwnerTable::Documents => "retired_at IS NULL",
        EmbeddingOwnerTable::Summaries | EmbeddingOwnerTable::Entities => "true",
    }
}

/// Queue `embed` jobs for every live row of the chosen tables (default: all six), optionally in one
/// space. The payload carries the target `model` for the worker.
pub async fn reindex(
    conn: &mut PgConnection,
    req: &ReindexRequest,
    default_model: &str,
) -> Result<ReindexResponse, ApiError> {
    let model = req.model.as_deref().unwrap_or(default_model);
    let tables: Vec<EmbeddingOwnerTable> = req.tables.clone().unwrap_or_else(|| EmbeddingOwnerTable::ALL.to_vec());
    let mut enqueued = 0_i64;
    for table in tables {
        let live = live_filter(table);
        let owners: Vec<Uuid> = sqlx::query_scalar(&format!(
            "SELECT id FROM {table} WHERE ($1::uuid IS NULL OR space_id = $1) AND {live} ORDER BY id"
        ))
        .bind(req.space_id)
        .fetch_all(&mut *conn)
        .await?;
        for chunk in owners.chunks(REINDEX_CHUNK) {
            let job_ids: Vec<Uuid> = chunk.iter().map(|_| new_id()).collect();
            sqlx::query(
                "INSERT INTO jobs (id, type, payload, priority)
                 SELECT j.id, 'embed', jsonb_build_object('owner_table', $1::text, 'owner_id', j.owner_id, 'model', $2::text),
                        $5
                   FROM unnest($3::uuid[], $4::uuid[]) AS j(id, owner_id)",
            )
            .bind(table.as_str())
            .bind(model)
            .bind(&job_ids)
            .bind(chunk)
            .bind(REINDEX_PRIORITY)
            .execute(&mut *conn)
            .await?;
            enqueued += i64::try_from(chunk.len()).unwrap_or(i64::MAX);
        }
    }
    Ok(ReindexResponse { jobs_enqueued: enqueued })
}

// ---------- project remap ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemapRequest {
    pub from: String,
    pub to: String,
    /// `None` = every space.
    #[serde(default)]
    pub space_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemapResponse {
    /// The audit run id; `POST /v1/admin/consolidation/{run_id}/revert` undoes the remap.
    pub run_id: Uuid,
    /// Rows moved, by table.
    pub rows: BTreeMap<String, i64>,
}

/// Tables carrying `(scope_kind, scope_key)`; sessions carry `project_key` instead.
const SCOPED_TABLES: [&str; 6] = ["observations", "summaries", "facts", "procedures", "work_state", "procedure_hints"];

/// Rename a project key: sessions' `project_key` and the `project` scope key of every memory row,
/// embedding and profile, in the given space or all. Each row is audited (`project.remap`, with
/// before/after) under a new run id. 409 if an active fact would collide with one already under
/// the new key. `actor` is recorded in the audit rows.
pub async fn remap_project(
    conn: &mut PgConnection,
    req: &RemapRequest,
    actor: &str,
) -> Result<RemapResponse, ApiError> {
    let (from, to) = (req.from.trim(), req.to.trim());
    if from.is_empty() || to.is_empty() || from == to {
        return Err(ApiError::invalid("from and to must be different, non-empty project keys"));
    }
    let collisions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM facts a JOIN facts b
            ON b.space_id = a.space_id AND b.subject_id = a.subject_id AND b.predicate = a.predicate
           AND b.scope_kind = 'project' AND b.scope_key = $2 AND b.status = 'active'
          WHERE a.scope_kind = 'project' AND a.scope_key = $1 AND a.status = 'active'
            AND ($3::uuid IS NULL OR a.space_id = $3)",
    )
    .bind(from)
    .bind(to)
    .bind(req.space_id)
    .fetch_one(&mut *conn)
    .await?;
    if collisions > 0 {
        return Err(ApiError::conflict(format!(
            "{collisions} active fact(s) of project {from:?} collide with active facts of {to:?}; consolidate them first"
        )));
    }
    let run_id = new_id();
    let mut rows: BTreeMap<String, i64> = BTreeMap::new();
    let mut spaces: Vec<Uuid> = Vec::new();

    let sessions: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, space_id FROM sessions WHERE project_key = $1 AND ($2::uuid IS NULL OR space_id = $2) ORDER BY id FOR UPDATE")
            .bind(from)
            .bind(req.space_id)
            .fetch_all(&mut *conn)
            .await?;
    for (id, space_id) in &sessions {
        let ctx = AuditCtx { space_id: *space_id, actor: actor.into(), run_id, model: None, prompt_version: None };
        let before = snapshot(conn, "sessions", *id).await?;
        sqlx::query("UPDATE sessions SET project_key = $2 WHERE id = $1").bind(id).bind(to).execute(&mut *conn).await?;
        let after = snapshot(conn, "sessions", *id).await?;
        record(conn, &ctx, "project.remap", "sessions", *id, before, after, Some(&format!("{from} -> {to}"))).await?;
        spaces.push(*space_id);
    }
    rows.insert("sessions".into(), sessions.len() as i64);

    for table in SCOPED_TABLES {
        let targets: Vec<(Uuid, Uuid)> = sqlx::query_as(&format!(
            "SELECT id, space_id FROM {table} WHERE scope_kind = 'project' AND scope_key = $1
                AND ($2::uuid IS NULL OR space_id = $2) ORDER BY id FOR UPDATE"
        ))
        .bind(from)
        .bind(req.space_id)
        .fetch_all(&mut *conn)
        .await?;
        for (id, space_id) in &targets {
            let ctx = AuditCtx { space_id: *space_id, actor: actor.into(), run_id, model: None, prompt_version: None };
            let before = snapshot(conn, table, *id).await?;
            sqlx::query(&format!("UPDATE {table} SET scope_key = $2 WHERE id = $1"))
                .bind(id)
                .bind(to)
                .execute(&mut *conn)
                .await?;
            let after = snapshot(conn, table, *id).await?;
            record(conn, &ctx, "project.remap", table, *id, before, after, Some(&format!("{from} -> {to}"))).await?;
            spaces.push(*space_id);
        }
        rows.insert(table.into(), targets.len() as i64);
    }
    let embeddings = sqlx::query(
        "UPDATE embeddings SET scope_key = $2 WHERE scope_kind = 'project' AND scope_key = $1
            AND ($3::uuid IS NULL OR space_id = $3)",
    )
    .bind(from)
    .bind(to)
    .bind(req.space_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    rows.insert("embeddings".into(), i64::try_from(embeddings).unwrap_or(i64::MAX));

    // Profiles are derived: drop the old key's, render the new key's.
    spaces.sort();
    spaces.dedup();
    for space in spaces {
        sqlx::query("DELETE FROM profiles WHERE space_id = $1 AND scope_kind = 'project' AND scope_key = $2")
            .bind(space)
            .bind(from)
            .execute(&mut *conn)
            .await?;
        notify_invalidate(conn, space, ScopeKind::Project, from).await?;
        profiles::refresh_scope(conn, space, ScopeKind::Project, to).await?;
        notify_invalidate(conn, space, ScopeKind::Project, to).await?;
    }
    Ok(RemapResponse { run_id, rows })
}
