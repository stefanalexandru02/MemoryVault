//! Bookkeeping of planner runs: which scopes were planned when (`scope_planning`), the log of runs
//! with their trigger (`planner_runs`), and the worker's `plan` jobs.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use memvault_core::jobs::{PlanJob, last_planned_at};
use memvault_core::schemas::common::{PlanTrigger, Scope, ScopeKind};
use memvault_core::{ApiError, AppState, system_tx};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{PlanOptions, ScopeFilter, Tally, run_planner_with};

/// Scopes of the space planned after `since`.
pub(super) async fn planned_since(
    conn: &mut PgConnection,
    space_id: Uuid,
    since: DateTime<Utc>,
) -> Result<Vec<Scope>, sqlx::Error> {
    let rows: Vec<(ScopeKind, String)> =
        sqlx::query_as("SELECT scope_kind, scope_key FROM scope_planning WHERE space_id = $1 AND last_planned_at > $2")
            .bind(space_id)
            .bind(since)
            .fetch_all(conn)
            .await?;
    Ok(rows.into_iter().map(|(kind, key)| Scope::new(kind, key)).collect())
}

fn counts(map: &BTreeMap<String, i64>) -> Value {
    serde_json::json!(map)
}

/// Log the run and stamp the scopes it planned: the one it was restricted to, else every scope of the
/// space that has observations or facts and was not left out.
pub(super) async fn record(
    conn: &mut PgConnection,
    space_id: Uuid,
    run_id: Uuid,
    trigger: PlanTrigger,
    filter: &ScopeFilter,
    now: DateTime<Utc>,
    tally: &Tally,
) -> Result<(), sqlx::Error> {
    let only = filter.restricted_to();
    sqlx::query(
        "INSERT INTO planner_runs (run_id, space_id, scope_kind, scope_key, trigger, packets, actions)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(run_id)
    .bind(space_id)
    .bind(only.map(|s| s.kind))
    .bind(only.map(|s| s.key.as_str()))
    .bind(trigger)
    .bind(counts(&tally.packets))
    .bind(counts(&tally.actions))
    .execute(&mut *conn)
    .await?;
    let stamp =
        "ON CONFLICT (space_id, scope_kind, scope_key) DO UPDATE SET last_planned_at = EXCLUDED.last_planned_at";
    if let Some(scope) = only {
        sqlx::query(&format!(
            "INSERT INTO scope_planning (space_id, scope_kind, scope_key, last_planned_at) VALUES ($1, $2, $3, $4) {stamp}"
        ))
        .bind(space_id)
        .bind(scope.kind)
        .bind(&scope.key)
        .bind(now)
        .execute(conn)
        .await?;
        return Ok(());
    }
    let scope = ScopeFilter::condition("s.scope_kind", "s.scope_key", 3);
    sqlx::query(&format!(
        "INSERT INTO scope_planning (space_id, scope_kind, scope_key, last_planned_at)
         SELECT $1, s.scope_kind, s.scope_key, $2
           FROM (SELECT scope_kind, scope_key FROM observations WHERE space_id = $1
                 UNION SELECT scope_kind, scope_key FROM facts WHERE space_id = $1
                 UNION SELECT scope_kind, scope_key FROM summaries WHERE space_id = $1) s
          WHERE {scope}
         {stamp}"
    ))
    .bind(space_id)
    .bind(now)
    .bind(filter.only_kind())
    .bind(filter.only_key())
    .bind(filter.skipped())
    .execute(conn)
    .await?;
    Ok(())
}

/// What the worker should do with a `plan` job after [`run_plan_job`].
#[derive(Debug)]
pub enum JobOutcome {
    /// The scope was planned (or no longer exists to plan).
    Planned,
    /// The scope was planned too recently: run the job again at this time.
    Deferred(DateTime<Utc>),
}

/// Run a `plan` job (payload [`PlanJob`]) as of `now`: the planner restricted to the job's scope, unless the
/// scope was planned within `CONSOLIDATE_MIN_INTERVAL_S` (by the nightly pass, an operator or an earlier
/// job), in which case the job is put off until the interval is over.
pub async fn run_plan_job(state: &AppState, payload: &Value, now: DateTime<Utc>) -> Result<JobOutcome, ApiError> {
    let job: PlanJob = serde_json::from_value(payload.clone())
        .map_err(|e| ApiError::internal(format!("invalid plan job payload: {e}")))?;
    let last = {
        let mut tx = system_tx(&state.db.system).await?;
        last_planned_at(&mut tx, job.space_id, &job.scope).await?
    };
    let min_interval =
        chrono::Duration::from_std(state.settings.consolidate_min_interval).unwrap_or(chrono::Duration::zero());
    if let Some(due) = last.map(|t| t + min_interval).filter(|due| *due > now) {
        metrics::counter!("memsvc_plan_jobs_total", "trigger" => job.trigger.as_str(), "outcome" => "deferred")
            .increment(1);
        return Ok(JobOutcome::Deferred(due));
    }
    let options = PlanOptions { space_id: Some(job.space_id), scope: Some(job.scope), trigger: job.trigger };
    run_planner_with(state, &options, now).await?;
    metrics::counter!("memsvc_plan_jobs_total", "trigger" => job.trigger.as_str(), "outcome" => "planned").increment(1);
    Ok(JobOutcome::Planned)
}
