//! Server-internal job queue rows (`jobs`, spec 4.6). The worker (memvault-worker) drains them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::ids::new_id;
use crate::schemas::common::{PlanTrigger, Scope};

/// Queue (re-)embedding of one row: `jobs(type='embed', payload={"owner_table", "owner_id"})`.
/// Call it in the transaction that writes the row. Re-exported as
/// `memvault_worker::embed::enqueue_embed`; ingest uses this path because it cannot depend on the
/// worker crate.
pub async fn enqueue_embed(conn: &mut PgConnection, owner_table: &str, owner_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO jobs (id, type, payload) VALUES ($1, 'embed', $2)")
        .bind(new_id())
        .bind(serde_json::json!({ "owner_table": owner_table, "owner_id": owner_id }))
        .execute(conn)
        .await?;
    Ok(())
}

/// `jobs.type` of a scoped planner run.
pub const PLAN_JOB: &str = "plan";

/// Payload of a `plan` job. The scope comes from rows the trigger saw (a session's observations), never
/// from a request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanJob {
    pub space_id: Uuid,
    pub scope: Scope,
    pub trigger: PlanTrigger,
}

/// When the planner last processed `scope`, if it ever did.
pub async fn last_planned_at(
    conn: &mut PgConnection,
    space_id: Uuid,
    scope: &Scope,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT last_planned_at FROM scope_planning WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3",
    )
    .bind(space_id)
    .bind(scope.kind)
    .bind(&scope.key)
    .fetch_optional(conn)
    .await
}

/// Queue a planner run for one scope (`jobs(type='plan')`, coalesced per scope). A scope with a plan job
/// waiting or running keeps it and this call is a no-op (`false`): the unique index `jobs_plan_live`
/// decides, so concurrent triggers cannot queue two. The minimum interval between runs of a scope is
/// enforced when the worker runs the job, which puts it back in the queue until the interval is over.
pub async fn enqueue_plan(
    conn: &mut PgConnection,
    space_id: Uuid,
    scope: &Scope,
    trigger: PlanTrigger,
) -> Result<bool, sqlx::Error> {
    let queued = sqlx::query("INSERT INTO jobs (id, type, payload) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(new_id())
        .bind(PLAN_JOB)
        .bind(serde_json::json!(PlanJob { space_id, scope: scope.clone(), trigger }))
        .execute(conn)
        .await?
        .rows_affected()
        == 1;
    let outcome = if queued { "queued" } else { "coalesced" };
    metrics::counter!("memsvc_plan_enqueued_total", "trigger" => trigger.as_str(), "outcome" => outcome).increment(1);
    Ok(queued)
}

/// [`enqueue_plan`] with the `threshold` trigger once `scope` holds at least `threshold` live observations
/// created after it was last planned. `threshold <= 0` turns the trigger off. The count stops at the
/// threshold, so it costs at most that many index entries.
pub async fn enqueue_plan_if_busy(
    conn: &mut PgConnection,
    space_id: Uuid,
    scope: &Scope,
    threshold: i64,
) -> Result<bool, sqlx::Error> {
    if threshold <= 0 {
        return Ok(false);
    }
    let since = last_planned_at(conn, space_id, scope).await?;
    let new: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT 1 FROM observations
                                WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND archived_at IS NULL
                                  AND ($4::timestamptz IS NULL OR created_at > $4)
                                LIMIT $5) n",
    )
    .bind(space_id)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(since)
    .bind(threshold)
    .fetch_one(&mut *conn)
    .await?;
    if new < threshold {
        return Ok(false);
    }
    enqueue_plan(conn, space_id, scope, PlanTrigger::Threshold).await
}
