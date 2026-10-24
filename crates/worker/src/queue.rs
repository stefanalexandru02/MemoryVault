//! The `jobs` table: claiming with `FOR UPDATE SKIP LOCKED`, completion, retry and lease expiry.

use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// A job is failed for good after this many attempts.
pub const MAX_ATTEMPTS: i32 = 5;
/// How long a claimed job stays locked before another worker may take it.
pub const LOCK_SECONDS: f64 = 300.0;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    pub id: Uuid,
    #[sqlx(rename = "type")]
    pub kind: String,
    pub payload: Value,
    pub attempts: i32,
}

/// Claim up to `limit` due jobs of the given types, oldest first.
pub async fn claim(pool: &PgPool, worker: &str, types: &[&str], limit: i64) -> Result<Vec<Job>, sqlx::Error> {
    let types: Vec<String> = types.iter().map(|t| t.to_string()).collect();
    sqlx::query_as(
        "UPDATE jobs SET status = 'running', locked_by = $1, attempts = attempts + 1,
                         locked_until = now() + make_interval(secs => $2)
          WHERE id IN (SELECT id FROM jobs WHERE status = 'queued' AND run_after <= now() AND type = ANY($3)
                        ORDER BY run_after, created_at LIMIT $4 FOR UPDATE SKIP LOCKED)
          RETURNING id, type, payload, attempts",
    )
    .bind(worker)
    .bind(LOCK_SECONDS)
    .bind(&types)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn complete(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE jobs SET status = 'done', locked_by = NULL, locked_until = NULL, last_error = NULL WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delay before attempt `attempts + 1`: 5 s doubling up to 5 min.
pub fn backoff(attempts: i32) -> Duration {
    Duration::from_secs((5u64 << attempts.clamp(0, 6) as u32).min(300))
}

/// Record a failure: requeue with backoff, or mark failed when `permanent` or out of attempts.
pub async fn fail(pool: &PgPool, job: &Job, error: &str, permanent: bool) -> Result<(), sqlx::Error> {
    let give_up = permanent || job.attempts >= MAX_ATTEMPTS;
    sqlx::query(
        "UPDATE jobs SET status = CASE WHEN $2 THEN 'failed' ELSE 'queued' END, last_error = $3,
                         run_after = now() + make_interval(secs => $4), locked_by = NULL, locked_until = NULL
          WHERE id = $1",
    )
    .bind(job.id)
    .bind(give_up)
    .bind(error)
    .bind(backoff(job.attempts).as_secs_f64())
    .execute(pool)
    .await?;
    Ok(())
}

/// Release jobs whose worker died (lock expired): requeue them, or fail those out of attempts.
pub async fn expire_locks(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE jobs SET status = CASE WHEN attempts >= $1 THEN 'failed' ELSE 'queued' END,
                         last_error = coalesce(last_error, 'lock expired'), locked_by = NULL, locked_until = NULL
          WHERE status = 'running' AND locked_until < now()",
    )
    .bind(MAX_ATTEMPTS)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Queue today's retention run; the idempotency key makes it once per UTC day across replicas.
pub async fn enqueue_daily_retention(pool: &PgPool) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "INSERT INTO jobs (id, type, payload, idempotency_key)
         VALUES (gen_random_uuid(), 'retention', '{}', 'retention:' || to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD'))
         ON CONFLICT (idempotency_key) DO NOTHING",
    )
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// Push the lock of running jobs `LOCK_SECONDS` into the future (heartbeat for long jobs).
pub async fn extend_locks(pool: &PgPool, ids: &[Uuid]) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE jobs SET locked_until = now() + make_interval(secs => $1) WHERE id = ANY($2) AND status = 'running'",
    )
    .bind(LOCK_SECONDS)
    .bind(ids)
    .execute(pool)
    .await?;
    Ok(())
}

/// Run `fut` while renewing the locks of `ids` every `LOCK_SECONDS / 3`, so a job that outlives
/// the lock is not requeued under a second worker.
pub async fn with_heartbeat<T>(pool: &PgPool, ids: &[Uuid], fut: impl std::future::Future<Output = T>) -> T {
    tokio::pin!(fut);
    let mut tick = tokio::time::interval(Duration::from_secs_f64(LOCK_SECONDS / 3.0));
    tick.tick().await;
    loop {
        tokio::select! {
            out = &mut fut => return out,
            _ = tick.tick() => {
                if let Err(e) = extend_locks(pool, ids).await {
                    tracing::warn!(error = %e, "lock heartbeat failed");
                }
            }
        }
    }
}
