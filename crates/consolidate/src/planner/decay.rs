//! Decay and archival (spec 12.1 step 2, architecture 5.4), in code only.
//!
//! `importance *= 0.5^(days / half_life)` applied incrementally: `decayed_at` records the last
//! decay, so a run decays only the time since the later of the last access/creation and the last
//! decay (the exponents add up to `days_since_last_access_or_creation / half_life`). Floors:
//! observations 0.0, facts 0.1 (a fact already below its floor is left alone). Pinned facts do not
//! decay. Observations below 0.05 importance and older than 60 days are archived and audited.

use chrono::{DateTime, Duration, Utc};
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::common::ScopeKind;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{AuditCtx, record, snapshot};

/// Half-life in days of `change` and `discovery` observations.
pub const HALF_LIFE_FLEETING_DAYS: f64 = 14.0;
/// Half-life of `decision` and `bugfix` observations.
pub const HALF_LIFE_DURABLE_DAYS: f64 = 90.0;
/// Half-life of every other observation type (spec 11.4: observations 30 d by default).
pub const HALF_LIFE_DEFAULT_DAYS: f64 = 30.0;
pub const HALF_LIFE_FACT_DAYS: f64 = 365.0;
pub const OBSERVATION_FLOOR: f64 = 0.0;
pub const FACT_FLOOR: f64 = 0.1;
pub const ARCHIVE_IMPORTANCE_BELOW: f64 = 0.05;
pub const ARCHIVE_AFTER_DAYS: i64 = 60;
/// A row decayed less than this long ago is skipped (the planner may run more than once a day).
pub const MIN_DECAY_INTERVAL_HOURS: i64 = 6;

pub fn half_life_days(observation_type: &str) -> f64 {
    match observation_type {
        "change" | "discovery" => HALF_LIFE_FLEETING_DAYS,
        "decision" | "bugfix" => HALF_LIFE_DURABLE_DAYS,
        _ => HALF_LIFE_DEFAULT_DAYS,
    }
}

/// `0.5^(elapsed_days / half_life_days)`.
pub fn decay_factor(elapsed_days: f64, half_life_days: f64) -> f64 {
    0.5_f64.powf(elapsed_days / half_life_days)
}

/// The decayed importance, never below `floor` (and never raised above what it was).
pub fn decayed_importance(importance: f64, elapsed_days: f64, half_life_days: f64, floor: f64) -> f64 {
    (importance * decay_factor(elapsed_days, half_life_days)).max(floor.min(importance))
}

/// Decay observations and facts of the space as of `now`; returns `(observations, facts)` updated.
pub async fn decay(conn: &mut PgConnection, space_id: Uuid, now: DateTime<Utc>) -> Result<(u64, u64), sqlx::Error> {
    let min_age = Duration::hours(MIN_DECAY_INTERVAL_HOURS).num_seconds() as f64;
    let observations = sqlx::query(
        "UPDATE observations SET
           importance = GREATEST(
             importance * power(0.5::float8, extract(epoch FROM ($2 - coalesce(decayed_at, created_at))) / 86400.0
               / (CASE type WHEN 'change' THEN $3 WHEN 'discovery' THEN $3 WHEN 'decision' THEN $4
                            WHEN 'bugfix' THEN $4 ELSE $5 END)),
             $6::float8)::real,
           decayed_at = $2
         WHERE space_id = $1 AND archived_at IS NULL
           AND extract(epoch FROM ($2 - coalesce(decayed_at, created_at))) >= $7",
    )
    .bind(space_id)
    .bind(now)
    .bind(HALF_LIFE_FLEETING_DAYS)
    .bind(HALF_LIFE_DURABLE_DAYS)
    .bind(HALF_LIFE_DEFAULT_DAYS)
    .bind(OBSERVATION_FLOOR)
    .bind(min_age)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    // ref = the later of last access/creation and last decay
    let facts = sqlx::query(
        "UPDATE facts SET
           importance = GREATEST(
             importance * power(0.5::float8, extract(epoch FROM ($2 - greatest(coalesce(last_accessed_at, first_seen_at),
                                                                              coalesce(decayed_at, first_seen_at)))) / 86400.0 / $3),
             LEAST(importance::float8, $4::float8))::real,
           decayed_at = $2
         WHERE space_id = $1 AND status IN ('active', 'candidate') AND NOT pinned
           AND extract(epoch FROM ($2 - greatest(coalesce(last_accessed_at, first_seen_at),
                                                 coalesce(decayed_at, first_seen_at)))) >= $5",
    )
    .bind(space_id)
    .bind(now)
    .bind(HALF_LIFE_FACT_DAYS)
    .bind(FACT_FLOOR)
    .bind(min_age)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok((observations, facts))
}

/// Archive observations below the importance threshold and past the age limit; each is audited
/// `obs.archive` with before/after snapshots. Returns how many.
pub async fn archive(conn: &mut PgConnection, ctx: &AuditCtx, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let rows: Vec<(Uuid, ScopeKind, String)> = sqlx::query_as(
        "SELECT id, scope_kind, scope_key FROM observations
          WHERE space_id = $1 AND archived_at IS NULL AND importance < $2 AND created_at < $3
          ORDER BY created_at, id FOR UPDATE",
    )
    .bind(ctx.space_id)
    .bind(ARCHIVE_IMPORTANCE_BELOW)
    .bind(now - Duration::days(ARCHIVE_AFTER_DAYS))
    .fetch_all(&mut *conn)
    .await?;
    for (id, kind, key) in &rows {
        let before = snapshot(conn, "observations", *id).await?;
        sqlx::query("UPDATE observations SET archived_at = $2 WHERE id = $1")
            .bind(id)
            .bind(now)
            .execute(&mut *conn)
            .await?;
        let after = snapshot(conn, "observations", *id).await?;
        record(
            conn,
            ctx,
            "obs.archive",
            "observations",
            *id,
            before,
            after,
            Some("importance decayed below threshold"),
        )
        .await?;
        notify_invalidate(conn, ctx.space_id, *kind, key).await?;
    }
    Ok(rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_lives_by_type() {
        assert_eq!(half_life_days("change"), 14.0);
        assert_eq!(half_life_days("discovery"), 14.0);
        assert_eq!(half_life_days("decision"), 90.0);
        assert_eq!(half_life_days("bugfix"), 90.0);
        assert_eq!(half_life_days("feature"), 30.0);
    }

    #[test]
    fn one_half_life_halves() {
        assert!((decay_factor(14.0, 14.0) - 0.5).abs() < 1e-12);
        assert!((decay_factor(28.0, 14.0) - 0.25).abs() < 1e-12);
        assert_eq!(decay_factor(0.0, 14.0), 1.0);
    }

    #[test]
    fn incremental_equals_total() {
        // two runs of 10 and 20 days decay like one run of 30
        let once = decayed_importance(0.8, 30.0, 30.0, 0.0);
        let twice = decayed_importance(decayed_importance(0.8, 10.0, 30.0, 0.0), 20.0, 30.0, 0.0);
        assert!((once - twice).abs() < 1e-12);
    }

    #[test]
    fn floors() {
        assert_eq!(decayed_importance(0.5, 10_000.0, 365.0, FACT_FLOOR), FACT_FLOOR);
        assert!(decayed_importance(0.5, 10_000.0, 14.0, OBSERVATION_FLOOR) < 1e-12);
        // already below the floor: unchanged, never raised
        assert_eq!(decayed_importance(0.05, 365.0, 365.0, FACT_FLOOR), 0.05);
    }
}
