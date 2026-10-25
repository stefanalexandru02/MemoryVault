//! Fixed one-minute window counters per token in `rate_limits` (spec 5.1). Fails open.

use axum::http::{StatusCode, header};
use sqlx::PgPool;

use crate::error::ApiError;

/// Count one request for `subject`; 429 `rate_limited` with `Retry-After` past `limit` per minute.
/// Any database error lets the request through (logged).
pub async fn check_rate_limit(pool: &PgPool, subject: &str, limit: i64) -> Result<(), ApiError> {
    let row: Result<(i32, i32), sqlx::Error> = sqlx::query_as(
        "INSERT INTO rate_limits (subject, window_start, count) VALUES ($1, date_trunc('minute', now()), 1)
         ON CONFLICT (subject, window_start) DO UPDATE SET count = rate_limits.count + 1
         RETURNING count,
           greatest(1, ceil(extract(epoch FROM window_start + interval '1 minute' - now())))::int",
    )
    .bind(subject)
    .fetch_one(pool)
    .await;
    match row {
        Err(e) => {
            tracing::warn!(error = %e, "rate limit check failed; allowing request");
            Ok(())
        }
        Ok((count, retry_after)) if i64::from(count) > limit => Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!("rate limit of {limit}/min exceeded"),
        )
        .retryable(true)
        .with_header(header::RETRY_AFTER, retry_after.to_string())),
        Ok(_) => Ok(()),
    }
}
