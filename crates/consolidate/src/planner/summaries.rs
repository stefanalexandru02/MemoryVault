//! Day and week summaries (spec 12.1 step 3).
//!
//! - A day packet for every `(scope, day)` of the last two weeks with at least two `session`
//!   summaries and no day summary yet; today is never summarised (the day is not over).
//! - On Sundays, a week packet per scope that has at least two day summaries (else two session
//!   summaries) in the seven days before the run (Sunday to Saturday, all complete days) and no
//!   week summary for them yet.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use memvault_core::schemas::common::{OutputSchemaName, PacketKind, ScopeKind};
use memvault_core::{ApiError, AppState};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::scope::ScopeFilter;
use super::{NewPacket, Tally, create_packet};
use crate::audit::AuditCtx;

/// How far back missing day summaries are looked for.
pub const LOOKBACK_DAYS: i64 = 14;
/// Summaries needed to justify a day or week packet.
pub const MIN_SUMMARIES: usize = 2;

/// The week a Sunday run summarises: the seven days before it. `None` on other weekdays.
pub fn week_period(today: NaiveDate) -> Option<(NaiveDate, NaiveDate)> {
    (today.weekday() == Weekday::Sun).then(|| (today - Duration::days(7), today - Duration::days(1)))
}

fn midnight(d: NaiveDate) -> DateTime<Utc> {
    d.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc()
}

const SUMMARY_JSON: &str =
    "jsonb_build_object('id', id, 'grain', grain, 'session_id', session_id, 'created_at', created_at,
        'request', request, 'investigated', investigated, 'learned', learned, 'completed', completed,
        'next_steps', next_steps, 'notes', notes)";

async fn session_summaries(
    conn: &mut PgConnection,
    space_id: Uuid,
    kind: ScopeKind,
    key: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Value>, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT {SUMMARY_JSON} FROM summaries
          WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND grain = 'session'
            AND created_at >= $4 AND created_at < $5 ORDER BY created_at, id"
    ))
    .bind(space_id)
    .bind(kind)
    .bind(key)
    .bind(from)
    .bind(to)
    .fetch_all(conn)
    .await
}

fn packet_inputs(
    ctx: &AuditCtx,
    grain: &str,
    kind: ScopeKind,
    key: &str,
    start: NaiveDate,
    end: NaiveDate,
    items: Vec<Value>,
) -> Value {
    json!({
        "run_id": ctx.run_id,
        "grain": grain,
        "scope_kind": kind,
        "scope_key": key,
        "scope": {"kind": kind, "key": key},
        "period": {"grain": grain, "start": start, "end": end},
        "summaries": items,
    })
}

async fn day_packets(
    conn: &mut PgConnection,
    state: &AppState,
    ctx: &AuditCtx,
    today: NaiveDate,
    filter: &ScopeFilter,
    tally: &mut Tally,
) -> Result<(), ApiError> {
    let days: Vec<(ScopeKind, String, NaiveDate)> = sqlx::query_as(
        "SELECT s.scope_kind, s.scope_key, (s.created_at AT TIME ZONE 'UTC')::date AS day
           FROM summaries s
          WHERE s.space_id = $1 AND s.grain = 'session' AND s.created_at >= $2 AND s.created_at < $3
            AND NOT EXISTS (SELECT 1 FROM summaries d
                             WHERE d.space_id = s.space_id AND d.grain = 'day' AND d.scope_kind = s.scope_kind
                               AND d.scope_key = s.scope_key
                               AND (d.period_start AT TIME ZONE 'UTC')::date = (s.created_at AT TIME ZONE 'UTC')::date)
          GROUP BY 1, 2, 3 HAVING count(*) >= $4
          ORDER BY 3, 1, 2",
    )
    .bind(ctx.space_id)
    .bind(midnight(today - Duration::days(LOOKBACK_DAYS)))
    .bind(midnight(today))
    .bind(i64::try_from(MIN_SUMMARIES).unwrap_or(2))
    .fetch_all(&mut *conn)
    .await?;
    for (kind, key, day) in days.into_iter().filter(|(kind, key, _)| filter.allows(*kind, key)) {
        let items =
            session_summaries(conn, ctx.space_id, kind, &key, midnight(day), midnight(day + Duration::days(1))).await?;
        let created = create_packet(
            conn,
            state,
            NewPacket {
                space_id: ctx.space_id,
                kind: PacketKind::ConsolidateSummary,
                prompt: "consolidate.summary",
                schema: OutputSchemaName::ConsolidateSummaryV1,
                inputs: packet_inputs(ctx, "day", kind, &key, day, day, items),
                idempotency_key: format!("consolidate_summary:{}:{kind}:{key}:day:{day}", ctx.space_id),
            },
        )
        .await?;
        if created {
            tally.packet(PacketKind::ConsolidateSummary);
        }
    }
    Ok(())
}

async fn week_packets(
    conn: &mut PgConnection,
    state: &AppState,
    ctx: &AuditCtx,
    (start, end): (NaiveDate, NaiveDate),
    filter: &ScopeFilter,
    tally: &mut Tally,
) -> Result<(), ApiError> {
    let (from, to) = (midnight(start), midnight(end + Duration::days(1)));
    let scopes: Vec<(ScopeKind, String)> = sqlx::query_as(
        "SELECT DISTINCT s.scope_kind, s.scope_key FROM summaries s
          WHERE s.space_id = $1 AND s.grain IN ('day', 'session')
            AND coalesce(s.period_start, s.created_at) >= $2 AND coalesce(s.period_start, s.created_at) < $3
            AND NOT EXISTS (SELECT 1 FROM summaries w
                             WHERE w.space_id = s.space_id AND w.grain = 'week' AND w.scope_kind = s.scope_kind
                               AND w.scope_key = s.scope_key AND w.period_start = $2)
          ORDER BY 1, 2",
    )
    .bind(ctx.space_id)
    .bind(from)
    .bind(to)
    .fetch_all(&mut *conn)
    .await?;
    for (kind, key) in scopes.into_iter().filter(|(kind, key)| filter.allows(*kind, key)) {
        let mut items: Vec<Value> = sqlx::query_scalar(&format!(
            "SELECT {SUMMARY_JSON} FROM summaries
              WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND grain = 'day'
                AND period_start >= $4 AND period_start < $5 ORDER BY period_start, id"
        ))
        .bind(ctx.space_id)
        .bind(kind)
        .bind(&key)
        .bind(from)
        .bind(to)
        .fetch_all(&mut *conn)
        .await?;
        if items.len() < MIN_SUMMARIES {
            items = session_summaries(conn, ctx.space_id, kind, &key, from, to).await?;
        }
        if items.len() < MIN_SUMMARIES {
            continue;
        }
        let created = create_packet(
            conn,
            state,
            NewPacket {
                space_id: ctx.space_id,
                kind: PacketKind::ConsolidateSummary,
                prompt: "consolidate.summary",
                schema: OutputSchemaName::ConsolidateSummaryV1,
                inputs: packet_inputs(ctx, "week", kind, &key, start, end, items),
                idempotency_key: format!("consolidate_summary:{}:{kind}:{key}:week:{start}", ctx.space_id),
            },
        )
        .await?;
        if created {
            tally.packet(PacketKind::ConsolidateSummary);
        }
    }
    Ok(())
}

pub async fn plan(
    conn: &mut PgConnection,
    state: &AppState,
    ctx: &AuditCtx,
    now: DateTime<Utc>,
    filter: &ScopeFilter,
    tally: &mut Tally,
) -> Result<(), ApiError> {
    let today = now.date_naive();
    day_packets(conn, state, ctx, today, filter, tally).await?;
    if let Some(week) = week_period(today) {
        week_packets(conn, state, ctx, week, filter, tally).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weeks_on_sundays_only() {
        let sunday = NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
        assert_eq!(sunday.weekday(), Weekday::Sun);
        let (start, end) = week_period(sunday).unwrap();
        assert_eq!((start.to_string(), end.to_string()), ("2026-10-18".into(), "2026-10-24".into()));
        assert_eq!(start.weekday(), Weekday::Sun);
        assert_eq!(end.weekday(), Weekday::Sat);
        for d in 19..=24 {
            assert!(week_period(NaiveDate::from_ymd_opt(2026, 10, d).unwrap()).is_none());
        }
    }
}
