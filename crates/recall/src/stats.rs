//! `GET /v1/stats/recall`: tokens served per call kind and injection mode, from `retrieval_log`, and the work
//! recalled ([`WorkRecalled`]), from the recalled ids of `access_log` and the memories' `work_tokens`.

use memvault_core::auth::ADMIN;
use memvault_core::schemas::recall::{ModeStats, RecallStats, StatsQuery, WorkRecalled};
use memvault_core::{ApiError, AppState, Identity, system_tx, user_tx};
use sqlx::PgConnection;
use uuid::Uuid;

pub const DEFAULT_DAYS: u32 = 7;
pub const MAX_DAYS: u32 = 90;

/// The caller's own calls over the window; `all` (admin tokens only) counts every user's.
pub async fn recall_stats(state: &AppState, identity: &Identity, q: &StatsQuery) -> Result<RecallStats, ApiError> {
    let days = q.days.unwrap_or(DEFAULT_DAYS);
    if !(1..=MAX_DAYS).contains(&days) {
        return Err(ApiError::invalid(format!("days must be between 1 and {MAX_DAYS}")));
    }
    let window = i32::try_from(days).unwrap_or(i32::MAX);
    let (modes, work) = if q.all {
        identity.ensure(ADMIN)?;
        let mut tx = system_tx(&state.db.system).await?;
        (query(&mut tx, window, None).await?, work(&mut tx, window, None).await?)
    } else {
        let mut tx = user_tx(&state.db.app, identity).await?;
        let owner = Some(identity.user_id);
        (query(&mut tx, window, owner).await?, work(&mut tx, window, owner).await?)
    };
    Ok(RecallStats { days, all_users: q.all, modes, work })
}

/// `owner = None` counts every client. Rows logged before tokens were recorded have none and are left out.
async fn query(conn: &mut PgConnection, days: i32, owner: Option<Uuid>) -> Result<Vec<ModeStats>, sqlx::Error> {
    sqlx::query_as(
        "SELECT mode, injection, count(*) AS calls, avg(tokens)::float8 AS avg_tokens,
                percentile_cont(0.5) WITHIN GROUP (ORDER BY tokens)::float8 AS p50_tokens,
                percentile_cont(0.95) WITHIN GROUP (ORDER BY tokens)::float8 AS p95_tokens,
                avg(hits)::float8 AS avg_hits
           FROM retrieval_log
          WHERE tokens IS NOT NULL AND created_at > now() - make_interval(days => $1)
            AND ($2::uuid IS NULL OR client_id IN (SELECT id FROM clients WHERE user_id = $2))
          GROUP BY mode, injection ORDER BY mode, injection NULLS FIRST",
    )
    .bind(days)
    .bind(owner)
    .fetch_all(conn)
    .await
}

/// Work recalled over the window: each (session, memory) of the recalled ids once, a call outside a session
/// keyed by its user and UTC day; a memory deleted since has no estimate.
async fn work(conn: &mut PgConnection, days: i32, owner: Option<Uuid>) -> Result<WorkRecalled, sqlx::Error> {
    let tokens_served: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(tokens), 0)::int8 FROM retrieval_log
          WHERE tokens IS NOT NULL AND created_at > now() - make_interval(days => $1)
            AND ($2::uuid IS NULL OR client_id IN (SELECT id FROM clients WHERE user_id = $2))",
    )
    .bind(days)
    .bind(owner)
    .fetch_one(&mut *conn)
    .await?;
    let (memories_recalled, work_tokens_recalled, memories_without_work_estimate): (i64, i64, i64) = sqlx::query_as(
        "WITH recalled AS (
               SELECT DISTINCT r.id,
                      coalesce(a.session_id::text, c.user_id::text || ':' || (a.created_at AT TIME ZONE 'UTC')::date)
                 FROM access_log a JOIN clients c ON c.id = a.client_id
                 CROSS JOIN LATERAL unnest(a.recalled_ids) AS r(id)
                WHERE a.recalled_ids <> '{}' AND a.created_at > now() - make_interval(days => $1)
                  AND ($2::uuid IS NULL OR c.user_id = $2))
             SELECT count(*), coalesce(sum(w.tokens), 0)::int8, count(*) FILTER (WHERE w.tokens IS NULL)
               FROM recalled r
               LEFT JOIN LATERAL (
                 SELECT work_tokens AS tokens FROM observations WHERE id = r.id
                 UNION ALL SELECT work_tokens FROM summaries WHERE id = r.id
                 UNION ALL SELECT work_tokens FROM facts WHERE id = r.id
                 UNION ALL SELECT work_tokens FROM procedures WHERE id = r.id
                 LIMIT 1) w ON true",
    )
    .bind(days)
    .bind(owner)
    .fetch_one(conn)
    .await?;
    let ratio = (tokens_served > 0).then(|| work_tokens_recalled as f64 / tokens_served as f64);
    Ok(WorkRecalled { tokens_served, memories_recalled, work_tokens_recalled, memories_without_work_estimate, ratio })
}
