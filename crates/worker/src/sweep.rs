//! The periodic sweep: idle windows and packet leases (ingest), job lock expiry, daily retention
//! scheduling and the queue gauges of spec 15.

use memvault_core::AppState;

use crate::queue;

/// One sweep. Each step logs its own failure so a broken step does not stop the others.
pub async fn sweep(state: &AppState) {
    let pool = &state.db.system;
    match memvault_ingest::windows::close_idle_windows(pool, &state.settings).await {
        Ok(n) if n > 0 => tracing::info!(closed = n, "closed idle windows"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "idle window sweep failed"),
    }
    match queue::expire_locks(pool).await {
        Ok(n) if n > 0 => tracing::warn!(expired = n, "job locks expired"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "job lock expiry failed"),
    }
    if let Err(e) = queue::enqueue_daily_retention(pool).await {
        tracing::warn!(error = %e, "scheduling retention failed");
    }
    if let Err(e) = update_gauges(pool).await {
        tracing::warn!(error = %e, "gauge update failed");
    }
}

/// `packets_pending{kind}`, `oldest_pending_packet_seconds`, `facts_active`, `facts_conflicts_pending`,
/// plus the job backlog.
pub async fn update_gauges(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    let pending: Vec<(String, i64)> =
        sqlx::query_as("SELECT kind, count(*) FROM work_packets WHERE status = 'pending' GROUP BY kind")
            .fetch_all(pool)
            .await?;
    for kind in [
        "extract",
        "summarize_prompt",
        "summarize_session",
        "consolidate_facts",
        "consolidate_summary",
        "consolidate_procedures",
    ] {
        let n = pending.iter().find(|(k, _)| k == kind).map_or(0, |(_, n)| *n);
        metrics::gauge!("memsvc_packets_pending", "kind" => kind).set(n as f64);
    }
    let (oldest, active, conflicts, queued): (f64, i64, i64, i64) = sqlx::query_as(
        "SELECT coalesce((SELECT extract(epoch FROM now() - min(created_at)) FROM work_packets WHERE status = 'pending'), 0)::float8,
                (SELECT count(*) FROM facts WHERE status = 'active'),
                (SELECT count(*) FROM facts WHERE status = 'candidate' AND conflict),
                (SELECT count(*) FROM jobs WHERE status = 'queued')",
    )
    .fetch_one(pool)
    .await?;
    metrics::gauge!("memsvc_oldest_pending_packet_seconds").set(oldest);
    metrics::gauge!("memsvc_facts_active").set(active as f64);
    metrics::gauge!("memsvc_facts_conflicts_pending").set(conflicts as f64);
    metrics::gauge!("memsvc_jobs_queued").set(queued as f64);
    Ok(())
}
