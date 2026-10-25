//! `GET /v1/stats/recall`: tokens served per call kind and injection mode, from `retrieval_log`.

use memvault_core::auth::ADMIN;
use memvault_core::schemas::recall::{ModeStats, RecallStats, StatsQuery};
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
    let modes = if q.all {
        identity.ensure(ADMIN)?;
        let mut tx = system_tx(&state.db.system).await?;
        query(&mut tx, days, None).await?
    } else {
        let mut tx = user_tx(&state.db.app, identity).await?;
        query(&mut tx, days, Some(identity.user_id)).await?
    };
    Ok(RecallStats { days, all_users: q.all, modes })
}

/// `owner = None` counts every client. Rows logged before tokens were recorded have none and are left out.
async fn query(conn: &mut PgConnection, days: u32, owner: Option<Uuid>) -> Result<Vec<ModeStats>, sqlx::Error> {
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
    .bind(i32::try_from(days).unwrap_or(i32::MAX))
    .bind(owner)
    .fetch_all(conn)
    .await
}
