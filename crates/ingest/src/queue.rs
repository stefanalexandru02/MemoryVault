//! Packet creation and the lease protocol (spec 6.6, 9.1, 9.3).
//!
//! Lease, heartbeat and fail run on the caller's `user_tx` connection (RLS: only packets in spaces
//! the caller can see are visible; the lease query also requires write access);
//! [`expire_leases`] runs on the worker sweep's `system_tx`.
//!
//! Attempt counting: `attempts` is incremented when a packet is leased (each lease is one attempt),
//! so a runner that dies without calling `/fail` still uses up an attempt. `/fail` releases the
//! lease with a backoff, or marks the packet `failed` when `attempts >= max_attempts`.

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use memvault_core::ApiError;
use memvault_core::ids::new_id;
use memvault_core::prompts::PromptRegistry;
use memvault_core::schemas::common::{OutputSchemaName, PacketKind, PacketStatus};
use memvault_core::schemas::work::{FailReason, LeaseRequest, LeasedPacket, PacketPrompt, PacketRecord};
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::windows;

/// Seconds before a released packet is offered again; doubled per attempt, capped at 6 h.
fn backoff_base_seconds(error: FailReason) -> i64 {
    match error {
        FailReason::QuotaExhausted => 900,
        FailReason::CliMissing => 1800,
        FailReason::Timeout | FailReason::Other => 60,
    }
}
pub const BACKOFF_MAX_SECONDS: i64 = 6 * 3600;

pub fn fail_reason_str(error: FailReason) -> &'static str {
    match error {
        FailReason::QuotaExhausted => "quota_exhausted",
        FailReason::CliMissing => "cli_missing",
        FailReason::Timeout => "timeout",
        FailReason::Other => "other",
    }
}

pub fn backoff_seconds(error: FailReason, attempts: i32) -> i64 {
    let exp = u32::try_from(attempts.saturating_sub(1).clamp(0, 20)).unwrap_or(0);
    (backoff_base_seconds(error) * 2_i64.pow(exp)).min(BACKOFF_MAX_SECONDS)
}

/// The failing device cannot run the packet at all: let any runner take it after the backoff.
fn drops_preferred(error: FailReason) -> bool {
    matches!(error, FailReason::QuotaExhausted | FailReason::CliMissing)
}

/// A new `work_packets` row.
#[derive(Debug, Clone)]
pub struct NewPacket<'a> {
    pub space_id: Uuid,
    pub kind: PacketKind,
    pub prompt_ref: &'a str,
    pub model_hint: &'a str,
    pub output_schema: OutputSchemaName,
    pub inputs: &'a Value,
    pub idempotency_key: &'a str,
    pub session_id: Option<Uuid>,
    pub preferred_client_id: Option<Uuid>,
    pub max_attempts: i32,
}

/// Insert a pending packet. Returns its id, or `None` when `idempotency_key` already exists.
pub async fn create_packet(conn: &mut PgConnection, p: NewPacket<'_>) -> Result<Option<Uuid>, sqlx::Error> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO work_packets (id, space_id, kind, session_id, preferred_client_id, prompt_ref, model_hint,
                                   inputs, output_schema, idempotency_key, max_attempts)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (idempotency_key) DO NOTHING
         RETURNING id",
    )
    .bind(new_id())
    .bind(p.space_id)
    .bind(p.kind)
    .bind(p.session_id)
    .bind(p.preferred_client_id)
    .bind(p.prompt_ref)
    .bind(p.model_hint)
    .bind(p.inputs)
    .bind(p.output_schema)
    .bind(p.idempotency_key)
    .bind(p.max_attempts)
    .fetch_optional(conn)
    .await?;
    if id.is_some() {
        metrics::counter!("memsvc_packets_created_total", "kind" => p.kind.as_str()).increment(1);
    }
    Ok(id)
}

/// A locked `work_packets` row with the ingest bookkeeping columns (migration 0100).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LockedPacket {
    #[sqlx(flatten)]
    pub record: PacketRecord,
    pub lease_seconds: Option<i32>,
    pub heartbeats: i32,
    pub applied: Option<Value>,
}

/// Lease up to `req.max` packets (spec 6.6, 9.3).
///
/// Order: packets preferring the caller, then unassigned ones (and other clients' packets once
/// `available_after + preferred_runner_seconds` has passed), then packets whose lease expired;
/// oldest first within a group. Concurrent callers never get the same packet (`SKIP LOCKED`).
/// A packet whose prompt cannot be resolved or rendered is marked `failed` and skipped.
pub async fn lease(
    conn: &mut PgConnection,
    prompts: &PromptRegistry,
    client_id: Uuid,
    req: &LeaseRequest,
    preferred_runner_seconds: i64,
) -> Result<Vec<LeasedPacket>, ApiError> {
    let max = usize::try_from(req.max).unwrap_or(1);
    let mut leased = Vec::new();
    while leased.len() < max {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM work_packets
              WHERE kind = ANY($1)
                AND ($2::uuid IS NULL OR session_id = $2)
                AND space_id IN (SELECT writable_spaces())
                AND attempts < max_attempts
                AND ((status = 'pending' AND available_after <= now())
                     OR (status = 'leased' AND lease_until < now()))
                AND (preferred_client_id IS NULL OR preferred_client_id = $3
                     OR available_after + make_interval(secs => $4) <= now())
              ORDER BY CASE WHEN status = 'leased' THEN 2
                            WHEN preferred_client_id = $3 THEN 0 ELSE 1 END,
                       available_after, created_at, id
              LIMIT $5
              FOR UPDATE SKIP LOCKED",
        )
        .bind(&req.kinds)
        .bind(req.session_id)
        .bind(client_id)
        .bind(preferred_runner_seconds as f64)
        .bind(i64::try_from(max - leased.len()).unwrap_or(1))
        .fetch_all(&mut *conn)
        .await?;
        if ids.is_empty() {
            break;
        }
        for id in ids {
            if let Some(packet) = take(conn, prompts, id, client_id, req).await? {
                leased.push(packet);
            }
        }
    }
    Ok(leased)
}

/// Label value for the runner kind a lease request reports: a short slug, else `unknown`/`other`
/// (the label must not take arbitrary client strings).
fn runner_kind(req: &LeaseRequest) -> String {
    match req.runner_kind.as_deref().map(str::trim) {
        None | Some("") => "unknown".into(),
        Some(k) if k.len() <= 24 && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => {
            k.to_string()
        }
        Some(_) => "other".into(),
    }
}

async fn take(
    conn: &mut PgConnection,
    prompts: &PromptRegistry,
    packet_id: Uuid,
    client_id: Uuid,
    req: &LeaseRequest,
) -> Result<Option<LeasedPacket>, ApiError> {
    let lease_seconds = req.lease_seconds;
    let row: PacketRecord = sqlx::query_as(
        "UPDATE work_packets SET status = 'leased', leased_by = $2, lease_until = now() + make_interval(secs => $3),
                lease_seconds = $4, heartbeats = 0, attempts = attempts + 1
          WHERE id = $1 RETURNING *",
    )
    .bind(packet_id)
    .bind(client_id)
    .bind(lease_seconds as f64)
    .bind(i32::try_from(lease_seconds).unwrap_or(i32::MAX))
    .fetch_one(&mut *conn)
    .await?;
    let rendered =
        prompts.get(&row.prompt_ref, None).and_then(|spec| prompts.render(spec, &row.inputs).map(|r| (spec, r)));
    let (spec, rendered) = match rendered {
        Ok(found) => found,
        Err(e) => {
            tracing::error!(packet = %packet_id, prompt = %row.prompt_ref, error = %e, "cannot render packet prompt");
            settle_failed(conn, Some(prompts), packet_id, &format!("prompt_error: {e}")).await?;
            return Ok(None);
        }
    };
    let prompt_ref = spec.prompt_ref();
    if prompt_ref != row.prompt_ref {
        // stored unpinned (no prompt loaded at creation): pin now
        sqlx::query("UPDATE work_packets SET prompt_ref = $2 WHERE id = $1")
            .bind(packet_id)
            .bind(&prompt_ref)
            .execute(&mut *conn)
            .await?;
    }
    metrics::counter!("memsvc_packets_leased_total", "kind" => row.kind.as_str(), "runner_kind" => runner_kind(req))
        .increment(1);
    Ok(Some(LeasedPacket {
        id: row.id,
        kind: row.kind,
        prompt_ref,
        model_hint: row.model_hint,
        inputs: row.inputs,
        output_schema: row.output_schema,
        lease_until: row.lease_until.expect("set by the lease update"),
        attempts: row.attempts,
        prompt: PacketPrompt {
            system: rendered.system,
            user_template: spec.user_template.clone(),
            user: rendered.user,
        },
    }))
}

/// Lock one packet (404 when invisible to the connection's role).
pub async fn lock_packet(conn: &mut PgConnection, packet_id: Uuid) -> Result<LockedPacket, ApiError> {
    sqlx::query_as("SELECT * FROM work_packets WHERE id = $1 FOR UPDATE")
        .bind(packet_id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| ApiError::not_found("packet not found"))
}

/// 409 unless `client_id` holds the packet's lease (an expired lease still counts until re-leased).
pub fn require_lease(packet: &PacketRecord, client_id: Uuid) -> Result<(), ApiError> {
    if packet.status != PacketStatus::Leased {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_leased",
            format!("packet is {}, not leased", packet.status),
        ));
    }
    if packet.leased_by != Some(client_id) {
        return Err(ApiError::new(StatusCode::CONFLICT, "lease_conflict", "packet is leased by another client"));
    }
    Ok(())
}

/// Extend the caller's lease once (spec 9.3) by its original duration.
pub async fn heartbeat(
    conn: &mut PgConnection,
    packet_id: Uuid,
    client_id: Uuid,
    default_seconds: i64,
) -> Result<(Uuid, DateTime<Utc>), ApiError> {
    let row = lock_packet(conn, packet_id).await?;
    require_lease(&row.record, client_id)?;
    if row.heartbeats >= 1 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "lease_extension_exhausted",
            "the lease was already extended once",
        ));
    }
    let seconds = row.lease_seconds.map_or(default_seconds, i64::from);
    Ok(sqlx::query_as(
        "UPDATE work_packets SET lease_until = now() + make_interval(secs => $2), heartbeats = heartbeats + 1
          WHERE id = $1 RETURNING id, lease_until",
    )
    .bind(packet_id)
    .bind(seconds as f64)
    .fetch_one(conn)
    .await?)
}

/// Release the caller's lease after a runner failure (spec 6.6). Returns the updated row.
pub async fn fail(
    conn: &mut PgConnection,
    prompts: &PromptRegistry,
    packet_id: Uuid,
    client_id: Uuid,
    error: FailReason,
    detail: Option<&str>,
) -> Result<PacketRecord, ApiError> {
    let row = lock_packet(conn, packet_id).await?;
    require_lease(&row.record, client_id)?;
    let error_str = fail_reason_str(error);
    let message = match detail {
        Some(d) if !d.is_empty() => format!("{error_str}: {d}"),
        _ => error_str.to_string(),
    };
    metrics::counter!("memsvc_packets_failed_total", "kind" => row.record.kind.as_str(), "error" => error_str)
        .increment(1);
    if row.record.attempts >= row.record.max_attempts {
        return Ok(settle_failed(conn, Some(prompts), packet_id, &message).await?);
    }
    Ok(sqlx::query_as(
        "UPDATE work_packets SET status = 'pending', lease_until = NULL, last_error = $2,
                available_after = now() + make_interval(secs => $3),
                preferred_client_id = CASE WHEN $4 THEN NULL ELSE preferred_client_id END
          WHERE id = $1 RETURNING *",
    )
    .bind(packet_id)
    .bind(&message)
    .bind(backoff_seconds(error, row.record.attempts) as f64)
    .bind(drops_preferred(error))
    .fetch_one(conn)
    .await?)
}

/// Mark a packet `failed` and create what waited on it (deferred summaries).
pub async fn settle_failed(
    conn: &mut PgConnection,
    prompts: Option<&PromptRegistry>,
    packet_id: Uuid,
    message: &str,
) -> Result<PacketRecord, sqlx::Error> {
    let row: PacketRecord = sqlx::query_as(
        "UPDATE work_packets SET status = 'failed', lease_until = NULL, last_error = $2, done_at = now()
          WHERE id = $1 RETURNING *",
    )
    .bind(packet_id)
    .bind(message)
    .fetch_one(&mut *conn)
    .await?;
    windows::after_packet_settled(conn, prompts, row.kind, row.session_id, &row.inputs).await?;
    Ok(row)
}

/// Fail packets whose lease expired on their last attempt (worker sweep). Each packet settles in
/// its own short transaction, so one failure cannot undo the others. Returns how many failed.
pub async fn expire_leases(pool: &PgPool, prompts: Option<&PromptRegistry>) -> Result<u64, sqlx::Error> {
    let mut tx = windows::sweep_tx(pool).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM work_packets WHERE status = 'leased' AND lease_until < now() AND attempts >= max_attempts",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut failed = 0;
    for id in ids {
        match expire_one(pool, prompts, id).await {
            Ok(true) => failed += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(packet = %id, error = %e, "expiring a packet lease failed"),
        }
    }
    Ok(failed)
}

/// Lock the packet (skipping one a runner is posting to, and re-checking that it is still expired)
/// and fail it.
async fn expire_one(pool: &PgPool, prompts: Option<&PromptRegistry>, id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = windows::sweep_tx(pool).await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM work_packets
          WHERE id = $1 AND status = 'leased' AND lease_until < now() AND attempts >= max_attempts
          FOR UPDATE SKIP LOCKED",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(false);
    }
    settle_failed(&mut tx, prompts, id, "lease expired on the last attempt").await?;
    tx.commit().await?;
    Ok(true)
}
