//! Spec 6.6, 9.3, 9.4 (work:lease): `POST /v1/work/lease`, `/v1/work/{id}/result`,
//! `/v1/work/{id}/fail`, `/v1/work/{id}/heartbeat`.
//!
//! Lease, fail and heartbeat run as the caller (`user_tx`): only packets in spaces the caller can
//! write are offered or visible. The result route runs in a `system_tx` (appliers write as the
//! system role) after checking that the packet's space is writable by the caller.

use axum::extract::State;
use memvault_core::auth::{self, writable_space_ids};
use memvault_core::http::{Json, Path};
use memvault_core::schemas::common::PacketStatus;
use memvault_core::schemas::validate::OutputError;
use memvault_core::schemas::work::{
    FailRequest, FailResponse, HeartbeatResponse, LeaseRequest, LeaseResponse, ResultRequest, ResultResponse,
};
use memvault_core::{ApiError, AppState, AppliedCounts, Identity, require_scope};
use uuid::Uuid;

use crate::builders::to_json;
use crate::{queue, windows};
use memvault_core::{system_tx, user_tx};

pub async fn lease(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<LeaseRequest>,
) -> Result<Json<LeaseResponse>, ApiError> {
    require_scope(&state, &identity, auth::WORK_LEASE).await?;
    body.validate()?;
    let mut tx = user_tx(&state.db.app, &identity).await?;
    let packets =
        queue::lease(&mut tx, &state.prompts, identity.client_id, &body, state.settings.preferred_runner_seconds)
            .await?;
    tx.commit().await?;
    Ok(Json(LeaseResponse { packets }))
}

/// Validate (fence stripping only, 9.4) and apply in one transaction. Idempotent on the packet: a
/// repeat post returns the first application's counts. Invalid output -> 422 `schema_invalid` with
/// `retry_with`; the lease is kept so the runner can retry.
pub async fn post_result(
    State(state): State<AppState>,
    identity: Identity,
    Path(packet_id): Path<Uuid>,
    Json(body): Json<ResultRequest>,
) -> Result<Json<ResultResponse>, ApiError> {
    require_scope(&state, &identity, auth::WORK_LEASE).await?;
    let mut tx = system_tx(&state.db.system).await?;
    let locked = queue::lock_packet(&mut tx, packet_id).await?;
    let mut packet = locked.record;
    if !writable_space_ids(&mut tx, identity.user_id).await?.contains(&packet.space_id) {
        return Err(ApiError::not_found("packet not found"));
    }
    if packet.status == PacketStatus::Done {
        let applied: AppliedCounts = locked.applied.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        return Ok(Json(ResultResponse { applied, duplicate: true }));
    }
    queue::require_lease(&packet, identity.client_id)?;
    let output = match state.schemas.validate(packet.output_schema, &body.output) {
        Ok(output) => output,
        Err(e @ OutputError::Invalid(_)) => {
            sqlx::query("UPDATE work_packets SET last_error = $2 WHERE id = $1")
                .bind(packet_id)
                .bind(format!("schema_invalid: {e}"))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            metrics::counter!("memsvc_packets_schema_invalid_total", "kind" => packet.kind.as_str()).increment(1);
            return Err(e.into());
        }
        Err(e) => return Err(e.into()),
    };
    let meta = to_json(&body.meta);
    packet.result_meta = Some(meta.clone());
    let counts = state.appliers.apply(&mut tx, &packet, &output).await?;
    sqlx::query(
        "UPDATE work_packets SET status = 'done', result = $2, result_meta = $3, applied = $4,
                done_at = now(), lease_until = NULL, last_error = NULL
          WHERE id = $1",
    )
    .bind(packet_id)
    .bind(to_json(&output))
    .bind(&meta)
    .bind(to_json(&counts))
    .execute(&mut *tx)
    .await?;
    windows::after_packet_settled(&mut tx, Some(&*state.prompts), packet.kind, packet.session_id, &packet.inputs)
        .await?;
    tx.commit().await?;
    metrics::counter!("memsvc_packets_done_total", "kind" => packet.kind.as_str()).increment(1);
    Ok(Json(ResultResponse { applied: counts, duplicate: false }))
}

pub async fn post_fail(
    State(state): State<AppState>,
    identity: Identity,
    Path(packet_id): Path<Uuid>,
    Json(body): Json<FailRequest>,
) -> Result<Json<FailResponse>, ApiError> {
    require_scope(&state, &identity, auth::WORK_LEASE).await?;
    let mut tx = user_tx(&state.db.app, &identity).await?;
    let row =
        queue::fail(&mut tx, &state.prompts, packet_id, identity.client_id, body.error, body.detail.as_deref()).await?;
    tx.commit().await?;
    Ok(Json(FailResponse {
        id: row.id,
        status: row.status,
        attempts: row.attempts,
        available_after: (row.status == PacketStatus::Pending).then_some(row.available_after),
    }))
}

pub async fn post_heartbeat(
    State(state): State<AppState>,
    identity: Identity,
    Path(packet_id): Path<Uuid>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    require_scope(&state, &identity, auth::WORK_LEASE).await?;
    let mut tx = user_tx(&state.db.app, &identity).await?;
    let (id, lease_until) =
        queue::heartbeat(&mut tx, packet_id, identity.client_id, state.settings.lease_seconds_default).await?;
    tx.commit().await?;
    Ok(Json(HeartbeatResponse { id, lease_until }))
}
