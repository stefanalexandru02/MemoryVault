//! Quality signals: `POST /v1/memories/{id}/feedback` and the MCP tool `memory_feedback`.
//!
//! A verdict (`helpful`, `wrong`, `outdated`) is stored append-only in `memory_feedback` with who, when
//! and an optional note; the unique `(user, memory, verdict)` index makes a repeat a no-op, so clicks do
//! not stack. A new verdict steers the memory, in the request's transaction and audited like any other
//! change:
//!
//! - fact: `helpful` nudges confidence up and sets `last_confirmed_at`; `wrong` and `outdated` lower
//!   confidence. When the net negative weight reaches the threshold the fact is queued (`disputed_at`) for
//!   the consolidation planner, which retracts it under a run id, so a revert undoes it. Pinned facts keep
//!   their confidence and are never queued;
//! - observation: the same step on `importance`;
//! - procedure: `helpful` counts a success, the others a failure.
//!
//! Summaries take no feedback. Nothing is deleted.

use axum::response::IntoResponse;
use memvault_core::auth::MEMORIES_WRITE;
use memvault_core::http::{Json, Path};
use memvault_core::ids::new_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::common::{FeedbackVerdict, MemoryTable, Scope};
use memvault_core::schemas::memories::{
    DISPUTE_THRESHOLD, FeedbackCounts, MemoryFeedbackRequest, MemoryFeedbackResponse,
};
use memvault_core::{ApiError, AppState, Identity, require_scope, user_tx};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::memories::{Audit, drop_profile, ensure_writable, locate, record_audit, snapshot};

/// Bounds of a fact's confidence after feedback (the consolidation merge cap is the same 0.98).
const CONFIDENCE_FLOOR: f64 = 0.05;
const CONFIDENCE_CAP: f64 = 0.98;

pub(crate) async fn route(
    axum::extract::State(state): axum::extract::State<AppState>,
    identity: Identity,
    Path(id): Path<Uuid>,
    Json(body): Json<MemoryFeedbackRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_WRITE).await?;
    Ok(Json(feedback(&state, &identity, id, body).await?))
}

/// `current` moved by `step`, kept within `[floor, cap]` and never pushed against the direction of the
/// step (a value already past a bound stays where it is).
fn nudge(current: f64, step: f64, floor: f64, cap: f64) -> f64 {
    let moved =
        if step >= 0.0 { current.max((current + step).min(cap)) } else { current.min((current + step).max(floor)) };
    round2(moved)
}

/// Two decimals: the stored `real` carries float noise (0.95 reads back as 0.949999988).
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Record `request` as a verdict of the caller on memory `id` and apply its effect once (spec: quality
/// signals). A verdict the caller gave before changes nothing and returns `applied: false`.
pub async fn feedback(
    state: &AppState,
    identity: &Identity,
    id: Uuid,
    request: MemoryFeedbackRequest,
) -> Result<MemoryFeedbackResponse, ApiError> {
    request.validate()?;
    let mut tx = user_tx(&state.db.app, identity).await?;
    let out = feedback_in(&mut tx, identity, id, &request).await?;
    tx.commit().await?;
    Ok(out)
}

async fn feedback_in(
    conn: &mut PgConnection,
    identity: &Identity,
    id: Uuid,
    request: &MemoryFeedbackRequest,
) -> Result<MemoryFeedbackResponse, ApiError> {
    let (table, space, kind, key) = locate(conn, id).await?;
    match table {
        MemoryTable::Summaries => {
            return Err(ApiError::invalid("summaries take no feedback; give it on the observations they rest on"));
        }
        MemoryTable::Documents => {
            return Err(ApiError::invalid("documents take no feedback; ingest the source again if it changed"));
        }
        _ => {}
    }
    ensure_writable(conn, identity, space).await?;
    // Verdicts on one memory are applied one at a time, so the counts below include every earlier one.
    sqlx::query(&format!("SELECT 1 FROM {table} WHERE id = $1 FOR UPDATE")).bind(id).execute(&mut *conn).await?;

    let verdict = request.verdict;
    let note = request.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let inserted = sqlx::query(
        "INSERT INTO memory_feedback (id, space_id, user_id, client_id, target_table, target_id, verdict, note)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (user_id, target_id, verdict) DO NOTHING",
    )
    .bind(new_id())
    .bind(space)
    .bind(identity.user_id)
    .bind(identity.client_id)
    .bind(table)
    .bind(id)
    .bind(verdict)
    .bind(note)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1;
    let counts: FeedbackCounts = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE verdict = 'helpful') AS helpful, count(*) FILTER (WHERE verdict = 'wrong') AS wrong,
                count(*) FILTER (WHERE verdict = 'outdated') AS outdated
           FROM memory_feedback WHERE target_id = $1",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;

    if inserted {
        let before = snapshot(conn, table, id).await?;
        apply(conn, table, id, verdict, &counts).await?;
        let reason = match note {
            Some(n) => format!("{verdict}: {n}"),
            None => verdict.to_string(),
        };
        let audit = Audit {
            space_id: space,
            action: audit_action(table),
            table,
            target_id: id,
            before,
            after: snapshot(conn, table, id).await?,
            reason: Some(&reason),
        };
        record_audit(conn, identity, audit).await?;
        let scope = Scope::new(kind, key);
        if table == MemoryTable::Facts {
            drop_profile(conn, space, &scope).await?;
        }
        notify_invalidate(conn, space, scope.kind, &scope.key).await?;
    }

    let (confidence, queued) = if table == MemoryTable::Facts {
        let (confidence, queued): (f64, bool) =
            sqlx::query_as("SELECT confidence::float8, disputed_at IS NOT NULL FROM facts WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *conn)
                .await?;
        (Some(round2(confidence)), queued)
    } else {
        (None, false)
    };
    Ok(MemoryFeedbackResponse { id, table, verdict, applied: inserted, counts, confidence, queued })
}

fn audit_action(table: MemoryTable) -> &'static str {
    match table {
        MemoryTable::Facts => "fact.feedback",
        MemoryTable::Observations => "obs.feedback",
        MemoryTable::Procedures => "proc.feedback",
        MemoryTable::Summaries => "sum.feedback",
        MemoryTable::Documents => "doc.feedback",
    }
}

/// The effect of a new verdict on the memory row; `counts` already include it.
async fn apply(
    conn: &mut PgConnection,
    table: MemoryTable,
    id: Uuid,
    verdict: FeedbackVerdict,
    counts: &FeedbackCounts,
) -> Result<(), sqlx::Error> {
    match table {
        MemoryTable::Facts => {
            let (confidence, status, pinned): (f64, String, bool) =
                sqlx::query_as("SELECT confidence::float8, status, pinned FROM facts WHERE id = $1")
                    .bind(id)
                    .fetch_one(&mut *conn)
                    .await?;
            // A fact that is no longer live keeps the verdict as a record only.
            if !matches!(status.as_str(), "candidate" | "active") {
                return Ok(());
            }
            let confidence =
                if pinned { confidence } else { nudge(confidence, verdict.step(), CONFIDENCE_FLOOR, CONFIDENCE_CAP) };
            let queue = status == "active" && !pinned && counts.dispute_score() >= DISPUTE_THRESHOLD;
            sqlx::query(
                "UPDATE facts SET confidence = $2::float8::float4,
                                  last_confirmed_at = CASE WHEN $3 THEN now() ELSE last_confirmed_at END,
                                  disputed_at = CASE WHEN $4 THEN coalesce(disputed_at, now()) END
                  WHERE id = $1",
            )
            .bind(id)
            .bind(confidence)
            .bind(verdict == FeedbackVerdict::Helpful)
            .bind(queue)
            .execute(conn)
            .await?;
        }
        MemoryTable::Observations => {
            let importance: f64 = sqlx::query_scalar("SELECT importance::float8 FROM observations WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *conn)
                .await?;
            sqlx::query("UPDATE observations SET importance = $2::float8::float4 WHERE id = $1")
                .bind(id)
                .bind(nudge(importance, verdict.step(), 0.0, 1.0))
                .execute(conn)
                .await?;
        }
        MemoryTable::Procedures => {
            let column = if verdict == FeedbackVerdict::Helpful { "success_count" } else { "failure_count" };
            sqlx::query(&format!("UPDATE procedures SET {column} = {column} + 1 WHERE id = $1"))
                .bind(id)
                .execute(conn)
                .await?;
        }
        // Rejected before any verdict is stored.
        MemoryTable::Summaries | MemoryTable::Documents => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudge_is_bounded_and_one_directional() {
        assert_eq!(nudge(0.95, 0.05, CONFIDENCE_FLOOR, CONFIDENCE_CAP), 0.98);
        assert_eq!(nudge(0.99, 0.05, CONFIDENCE_FLOOR, CONFIDENCE_CAP), 0.99, "already past the cap: not lowered");
        assert_eq!(nudge(0.6, -0.2, CONFIDENCE_FLOOR, CONFIDENCE_CAP), 0.4);
        assert_eq!(nudge(0.1, -0.2, CONFIDENCE_FLOOR, CONFIDENCE_CAP), 0.05);
        assert_eq!(nudge(0.02, -0.2, CONFIDENCE_FLOOR, CONFIDENCE_CAP), 0.02, "already under the floor: not raised");
        assert_eq!(nudge(f64::from(0.95_f32), -0.2, CONFIDENCE_FLOOR, CONFIDENCE_CAP), 0.75);
        assert_eq!(nudge(0.98, 0.05, 0.0, 1.0), 1.0);
    }
}
