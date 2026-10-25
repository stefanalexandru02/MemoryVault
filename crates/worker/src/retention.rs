//! Retention (spec 4.8), run by the daily `retention` job.

use memvault_core::Settings;
use sqlx::PgPool;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RetentionReport {
    pub events_trimmed: u64,
    pub packets_deleted: u64,
    pub logs_deleted: u64,
    pub rate_limits_deleted: u64,
    pub jobs_deleted: u64,
}

/// Trim old event payloads to `{"kind", "excerpt"}` (rows are kept: observations reference them),
/// delete settled work packets and old log rows (including planner runs), and prune finished jobs and rate-limit windows.
pub async fn run_retention(pool: &PgPool, settings: &Settings) -> Result<RetentionReport, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let events_trimmed = sqlx::query(
        "UPDATE events SET payload = jsonb_build_object('kind', kind, 'excerpt', left(payload::text, 500)),
                           payload_trimmed = true
          WHERE NOT payload_trimmed AND received_at < now() - make_interval(days => $1::int)",
    )
    .bind(settings.retention_events_payload_days as i32)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    let old_packets = "SELECT id FROM work_packets WHERE status IN ('done', 'failed')
                          AND coalesce(done_at, created_at) < now() - make_interval(days => $1::int)";
    sqlx::query(&format!("UPDATE procedure_hints SET packet_id = NULL WHERE packet_id IN ({old_packets})"))
        .bind(settings.retention_packets_days as i32)
        .execute(&mut *tx)
        .await?;
    let packets_deleted = sqlx::query(&format!("DELETE FROM work_packets WHERE id IN ({old_packets})"))
        .bind(settings.retention_packets_days as i32)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let mut logs_deleted = 0;
    for table in ["retrieval_log", "access_log", "planner_runs"] {
        logs_deleted +=
            sqlx::query(&format!("DELETE FROM {table} WHERE created_at < now() - make_interval(days => $1::int)"))
                .bind(settings.retention_logs_days as i32)
                .execute(&mut *tx)
                .await?
                .rows_affected();
    }
    let rate_limits_deleted = sqlx::query("DELETE FROM rate_limits WHERE window_start < now() - interval '1 day'")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let jobs_deleted =
        sqlx::query("DELETE FROM jobs WHERE status IN ('done', 'failed') AND created_at < now() - interval '7 days'")
            .execute(&mut *tx)
            .await?
            .rows_affected();
    tx.commit().await?;
    Ok(RetentionReport { events_trimmed, packets_deleted, logs_deleted, rate_limits_deleted, jobs_deleted })
}
