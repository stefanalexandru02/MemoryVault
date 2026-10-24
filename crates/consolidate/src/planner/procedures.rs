//! Procedure mining (spec 12.1 step 4): unconsumed `procedure_hints` grouped by
//! `(scope, task_class)` with at least 3 hints from at least 2 sessions become a
//! `consolidate_procedures` packet carrying the hints, the observations they came from and the
//! existing active procedure for the class, if any.

use memvault_core::schemas::common::{OutputSchemaName, PacketKind, ScopeKind};
use memvault_core::{ApiError, AppState};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{NewPacket, Tally, create_packet};
use crate::audit::AuditCtx;

pub const MIN_HINTS: i64 = 3;
pub const MIN_SESSIONS: i64 = 2;
/// Hints and source observations listed in one packet.
const MAX_HINTS: i64 = 30;
const MAX_OBSERVATIONS: i64 = 12;

pub async fn plan(
    conn: &mut PgConnection,
    state: &AppState,
    ctx: &AuditCtx,
    tally: &mut Tally,
) -> Result<(), ApiError> {
    let space_id = ctx.space_id;
    let groups: Vec<(ScopeKind, String, String)> = sqlx::query_as(
        "SELECT scope_kind, scope_key, task_class FROM procedure_hints
          WHERE space_id = $1 AND consumed_at IS NULL
          GROUP BY 1, 2, 3 HAVING count(*) >= $2 AND count(DISTINCT session_id) >= $3
          ORDER BY 1, 2, 3",
    )
    .bind(space_id)
    .bind(MIN_HINTS)
    .bind(MIN_SESSIONS)
    .fetch_all(&mut *conn)
    .await?;
    for (kind, key, task_class) in groups {
        let group = format!("{kind}:{key}|{task_class}");
        let open: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM work_packets WHERE space_id = $1 AND kind = 'consolidate_procedures'
                              AND status IN ('pending', 'leased') AND inputs ->> 'group' = $2)",
        )
        .bind(space_id)
        .bind(&group)
        .fetch_one(&mut *conn)
        .await?;
        if open {
            continue;
        }
        let hints: Vec<Value> = sqlx::query_scalar(
            "SELECT jsonb_build_object('id', id, 'note', note, 'observation_id', observation_id,
                                       'session_id', session_id, 'created_at', created_at)
               FROM procedure_hints
              WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND task_class = $4 AND consumed_at IS NULL
              ORDER BY created_at, id LIMIT $5",
        )
        .bind(space_id)
        .bind(kind)
        .bind(&key)
        .bind(&task_class)
        .bind(MAX_HINTS)
        .fetch_all(&mut *conn)
        .await?;
        let hint_ids: Vec<&str> = hints.iter().filter_map(|h| h["id"].as_str()).collect();
        let observation_ids: Vec<Uuid> =
            hints.iter().filter_map(|h| h["observation_id"].as_str()?.parse().ok()).collect();
        let observations: Vec<Value> = sqlx::query_scalar(
            "SELECT jsonb_build_object('id', id, 'type', type, 'title', title, 'narrative', narrative,
                                       'facts', facts, 'files', files, 'created_at', created_at)
               FROM observations WHERE space_id = $1 AND id = ANY($2) ORDER BY created_at, id LIMIT $3",
        )
        .bind(space_id)
        .bind(&observation_ids)
        .bind(MAX_OBSERVATIONS)
        .fetch_all(&mut *conn)
        .await?;
        let existing: Option<Value> = sqlx::query_scalar(
            "SELECT jsonb_build_object('id', id, 'name', name, 'version', version, 'trigger_text', trigger_text,
                                       'body_md', body_md)
               FROM procedures WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND task_class = $4
                AND status = 'active' ORDER BY version DESC LIMIT 1",
        )
        .bind(space_id)
        .bind(kind)
        .bind(&key)
        .bind(&task_class)
        .fetch_optional(&mut *conn)
        .await?;
        let digest = Sha256::digest(format!("{group}|{}", hint_ids.join(",")).as_bytes());
        let inputs = json!({
            "run_id": ctx.run_id,
            "group": group,
            "scope_kind": kind,
            "scope_key": key,
            "scope": {"kind": kind, "key": key},
            "task_class": task_class,
            "hints": hints,
            "observations": observations,
            "existing_procedure": existing,
        });
        let created = create_packet(
            conn,
            state,
            NewPacket {
                space_id,
                kind: PacketKind::ConsolidateProcedures,
                prompt: "consolidate.procedures",
                schema: OutputSchemaName::ConsolidateProceduresV1,
                inputs,
                idempotency_key: format!("consolidate_procedures:{space_id}:{}", &hex::encode(digest)[..24]),
            },
        )
        .await?;
        if created {
            tally.packet(PacketKind::ConsolidateProcedures);
        }
    }
    Ok(())
}
