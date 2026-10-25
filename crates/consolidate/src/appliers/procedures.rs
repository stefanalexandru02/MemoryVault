//! `consolidate_procedures` (spec 12.3): a new procedure, or a revision that creates a new version
//! row and retires the old one. The hints the packet carried are consumed either way.

use async_trait::async_trait;
use memvault_core::appliers::{AppliedCounts, PacketApplier};
use memvault_core::ids::new_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::jobs::enqueue_embed;
use memvault_core::schemas::common::ScopeKind;
use memvault_core::schemas::outputs::{ConsolidateProceduresV1, LlmOutput};
use memvault_core::schemas::work::PacketRecord;
use memvault_core::{ApiError, estimate_tokens};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{packet_ctx, run_id_of, wrong_output};
use crate::audit::{record, snapshot};

pub struct ProceduresApplier;

#[async_trait]
impl PacketApplier for ProceduresApplier {
    async fn apply(
        &self,
        conn: &mut PgConnection,
        packet: &PacketRecord,
        output: &LlmOutput,
    ) -> Result<AppliedCounts, ApiError> {
        let LlmOutput::ConsolidateProcedures(out) = output else {
            return Err(wrong_output(packet, output));
        };
        apply(conn, packet, out).await
    }
}

fn input_str<'a>(packet: &'a PacketRecord, key: &str) -> Option<&'a str> {
    packet.inputs.get(key)?.as_str()
}

fn input_uuids(packet: &PacketRecord, pointer: &str, field: &str) -> Vec<Uuid> {
    packet
        .inputs
        .pointer(pointer)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| if field.is_empty() { i.as_str() } else { i.get(field)?.as_str() })
                .filter_map(|s| s.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

async fn consume_hints(
    conn: &mut PgConnection,
    packet: &PacketRecord,
    hint_ids: &[Uuid],
    counts: &mut AppliedCounts,
) -> Result<(), ApiError> {
    let consumed = sqlx::query(
        "UPDATE procedure_hints SET consumed_at = now(), consumed_run_id = $3
      WHERE space_id = $1 AND id = ANY($2) AND consumed_at IS NULL",
    )
    .bind(packet.space_id)
    .bind(hint_ids)
    .bind(run_id_of(packet))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    counts.insert("hints_consumed".into(), i64::try_from(consumed).unwrap_or(i64::MAX));
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct OldProcedure {
    version: i32,
    status: String,
}

async fn apply(
    conn: &mut PgConnection,
    packet: &PacketRecord,
    out: &ConsolidateProceduresV1,
) -> Result<AppliedCounts, ApiError> {
    let mut counts = AppliedCounts::new();
    let hint_ids = input_uuids(packet, "/hints", "id");
    if out.skip {
        consume_hints(conn, packet, &hint_ids, &mut counts).await?;
        counts.insert("skipped".into(), 1);
        return Ok(counts);
    }
    let (Some(name), Some(trigger_text), Some(body_md)) = (&out.name, &out.trigger_text, &out.body_md) else {
        return Err(ApiError::invalid(
            "consolidate.procedures.v1 without skip needs name, task_class, trigger_text and body_md",
        ));
    };
    if out.task_class.is_none() {
        return Err(ApiError::invalid(
            "consolidate.procedures.v1 without skip needs name, task_class, trigger_text and body_md",
        ));
    }
    // scope and task class come from the packet, never from the model
    let task_class = input_str(packet, "task_class")
        .or(out.task_class.as_deref())
        .ok_or_else(|| ApiError::internal("consolidate_procedures packet without inputs.task_class"))?;
    let scope_kind: ScopeKind = input_str(packet, "scope_kind")
        .ok_or_else(|| ApiError::internal("consolidate_procedures packet without inputs.scope_kind"))?
        .parse()
        .map_err(|e| ApiError::internal(format!("consolidate_procedures packet: {e}")))?;
    let scope_key = input_str(packet, "scope_key").unwrap_or_default();
    let ctx = packet_ctx(packet);

    // A revision may only target the procedure the packet carried; with one carried, the output
    // revises it even when the model left supersedes_id out.
    let existing: Option<Uuid> =
        packet.inputs.pointer("/existing_procedure/id").and_then(|v| v.as_str()).and_then(|s| s.parse().ok());
    let claimed: Option<Uuid> = match out.supersedes_id.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => Some(s.parse().map_err(|_| ApiError::invalid(format!("supersedes_id {s:?} is not a UUID")))?),
        None => None,
    };
    let target = match (existing, claimed) {
        (Some(e), Some(c)) if e != c => {
            tracing::warn!(packet = %packet.id, "procedure revision rejected: {c} is not the packet's procedure {e}");
            counts.insert("rejected".into(), 1);
            return Ok(counts);
        }
        (None, Some(c)) => {
            tracing::warn!(packet = %packet.id, "procedure revision rejected: packet carries no procedure, got {c}");
            counts.insert("rejected".into(), 1);
            return Ok(counts);
        }
        (Some(e), _) => Some(e),
        (None, None) => None,
    };

    let mut version = 1;
    let mut supersedes: Option<Uuid> = None;
    if let Some(old_id) = target {
        let row: Option<OldProcedure> = sqlx::query_as(
            "SELECT version, status FROM procedures
              WHERE id = $1 AND space_id = $2 AND scope_kind = $3 AND scope_key = $4 AND task_class = $5 FOR UPDATE",
        )
        .bind(old_id)
        .bind(packet.space_id)
        .bind(scope_kind)
        .bind(scope_key)
        .bind(task_class)
        .fetch_optional(&mut *conn)
        .await?;
        match row {
            Some(r) if r.status == "active" => {
                version = r.version + 1;
                supersedes = Some(old_id);
                let before = snapshot(conn, "procedures", old_id).await?;
                sqlx::query("UPDATE procedures SET status = 'retired' WHERE id = $1")
                    .bind(old_id)
                    .execute(&mut *conn)
                    .await?;
                let after = snapshot(conn, "procedures", old_id).await?;
                record(
                    conn,
                    &ctx,
                    "proc.retire",
                    "procedures",
                    old_id,
                    before,
                    after,
                    Some("revised by a newer version"),
                )
                .await?;
            }
            _ => {
                tracing::warn!(packet = %packet.id, "procedure revision rejected: {old_id} is not an active procedure of the packet's class");
                counts.insert("rejected".into(), 1);
                return Ok(counts);
            }
        }
    }
    consume_hints(conn, packet, &hint_ids, &mut counts).await?;
    let id = new_id();
    let sources = input_uuids(packet, "/hints", "observation_id");
    sqlx::query(
        "INSERT INTO procedures (id, space_id, scope_kind, scope_key, name, task_class, trigger_text, body_md, version,
                                 supersedes_id, source_observation_ids, token_estimate)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(id)
    .bind(packet.space_id)
    .bind(scope_kind)
    .bind(scope_key)
    .bind(name)
    .bind(task_class)
    .bind(trigger_text)
    .bind(body_md)
    .bind(version)
    .bind(supersedes)
    .bind(&sources)
    .bind(estimate_tokens(body_md))
    .execute(&mut *conn)
    .await?;
    let after = snapshot(conn, "procedures", id).await?;
    let action = if supersedes.is_some() { "proc.revise" } else { "proc.create" };
    record(conn, &ctx, action, "procedures", id, None, after, Some("consolidated from procedure hints")).await?;
    enqueue_embed(conn, "procedures", id).await?;
    notify_invalidate(conn, packet.space_id, scope_kind, scope_key).await?;
    counts.insert(if supersedes.is_some() { "procedures_revised" } else { "procedures_created" }.into(), 1);
    Ok(counts)
}
