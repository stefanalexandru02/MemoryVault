//! `consolidate_summary` (spec 12.3): stores a day or week summary. Scope, grain and period come
//! from the packet inputs the planner built; the model only supplies the text.

use async_trait::async_trait;
use chrono::{Duration, NaiveDate};
use memvault_core::appliers::{AppliedCounts, PacketApplier};
use memvault_core::ids::new_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::jobs::enqueue_embed;
use memvault_core::schemas::common::{ScopeKind, SummaryGrain};
use memvault_core::schemas::outputs::{ConsolidateSummaryV1, LlmOutput};
use memvault_core::schemas::work::PacketRecord;
use memvault_core::{ApiError, estimate_tokens};
use sqlx::PgConnection;

use super::{extracted_by, packet_ctx, wrong_output};
use crate::audit::{record, snapshot};

pub struct SummaryApplier;

#[async_trait]
impl PacketApplier for SummaryApplier {
    async fn apply(
        &self,
        conn: &mut PgConnection,
        packet: &PacketRecord,
        output: &LlmOutput,
    ) -> Result<AppliedCounts, ApiError> {
        let LlmOutput::ConsolidateSummary(out) = output else {
            return Err(wrong_output(packet, output));
        };
        apply(conn, packet, out).await
    }
}

fn input_str<'a>(packet: &'a PacketRecord, key: &str) -> Option<&'a str> {
    packet.inputs.get(key)?.as_str()
}

fn input_date(packet: &PacketRecord, key: &str) -> Option<NaiveDate> {
    packet.inputs.pointer(&format!("/period/{key}"))?.as_str()?.parse().ok()
}

async fn apply(
    conn: &mut PgConnection,
    packet: &PacketRecord,
    out: &ConsolidateSummaryV1,
) -> Result<AppliedCounts, ApiError> {
    let grain = match input_str(packet, "grain") {
        Some("week") => SummaryGrain::Week,
        Some("day") => SummaryGrain::Day,
        _ => match out.period.grain {
            Some(memvault_core::schemas::outputs::PeriodGrain::Week) => SummaryGrain::Week,
            _ => SummaryGrain::Day,
        },
    };
    let scope_kind: ScopeKind = input_str(packet, "scope_kind")
        .ok_or_else(|| ApiError::internal("consolidate_summary packet without inputs.scope_kind"))?
        .parse()
        .map_err(|e| ApiError::internal(format!("consolidate_summary packet: {e}")))?;
    let scope_key = input_str(packet, "scope_key").unwrap_or_default();
    let start = input_date(packet, "start").unwrap_or(out.period.start);
    let end = input_date(packet, "end").unwrap_or(out.period.end);
    if end < start {
        return Err(ApiError::internal("consolidate_summary packet with end before start"));
    }
    if (start, end) != (out.period.start, out.period.end) {
        tracing::warn!(packet = %packet.id, "model period differs from the packet's; using the packet's");
    }
    let s = &out.summary;
    let search = [&s.request, &s.learned, &s.completed].map(|v| v.as_deref().unwrap_or_default()).join(" ");
    let tokens = estimate_tokens(&format!("{search} {} {}", s.next_steps, s.notes.as_deref().unwrap_or_default()));
    let id = new_id();
    sqlx::query(
        "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, period_start, period_end, request,
                                investigated, learned, completed, next_steps, notes, extracted_by, token_estimate)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(id)
    .bind(packet.space_id)
    .bind(scope_kind)
    .bind(scope_key)
    .bind(grain)
    .bind(start.and_hms_opt(0, 0, 0).map(|t| t.and_utc()))
    // exclusive end: midnight after the last covered day
    .bind((end + Duration::days(1)).and_hms_opt(0, 0, 0).map(|t| t.and_utc()))
    .bind(&s.request)
    .bind(&s.investigated)
    .bind(&s.learned)
    .bind(&s.completed)
    .bind(&s.next_steps)
    .bind(&s.notes)
    .bind(extracted_by(packet))
    .bind(tokens)
    .execute(&mut *conn)
    .await?;
    let after = snapshot(conn, "summaries", id).await?;
    let action = if grain == SummaryGrain::Week { "sum.create.week" } else { "sum.create.day" };
    record(conn, &packet_ctx(packet), action, "summaries", id, None, after, Some("consolidated summary")).await?;
    enqueue_embed(conn, "summaries", id).await?;
    notify_invalidate(conn, packet.space_id, scope_kind, scope_key).await?;
    Ok(AppliedCounts::from([("summaries".to_string(), 1)]))
}
