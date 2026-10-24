//! `consolidate_facts` (spec 12.2): applies the model's decisions on a fact cluster.
//!
//! Rules enforced here, not by the model:
//! - every id must belong to the packet's cluster (`inputs.fact_ids`) and to the packet's space;
//! - a decision that violates a rule is rejected (counted under `rejected`, logged) and changes nothing;
//! - pinned facts are never superseded, retracted, promoted or re-scored, and their confidence is
//!   kept on merge;
//! - `retract` must cite at least one existing observation id in its reason;
//! - `promote_scope` stays in the space and only widens a scope to `global` (a project, device or
//!   agent key cannot be derived from a decision);
//! - confidence after a merge is `1 - (1 - c_target)(1 - c_candidate)`, capped at 0.98.

use std::collections::{BTreeSet, HashSet};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use memvault_core::ApiError;
use memvault_core::appliers::{AppliedCounts, PacketApplier};
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::common::{FactStatus, ScopeHint, ScopeKind};
use memvault_core::schemas::outputs::{ConsolidateFactsV1, FactDecision, LlmOutput};
use memvault_core::schemas::work::PacketRecord;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{packet_ctx, wrong_output};
use crate::audit::{AuditCtx, record, snapshot};
use crate::profiles;

/// Confidence ceiling of a merge (spec 12.2).
pub const MERGE_CONFIDENCE_CAP: f64 = 0.98;

pub struct FactsApplier;

#[async_trait]
impl PacketApplier for FactsApplier {
    async fn apply(
        &self,
        conn: &mut PgConnection,
        packet: &PacketRecord,
        output: &LlmOutput,
    ) -> Result<AppliedCounts, ApiError> {
        let LlmOutput::ConsolidateFacts(out) = output else {
            return Err(wrong_output(packet, output));
        };
        apply_decisions(conn, packet, out).await
    }
}

/// `1 - (1 - a)(1 - b)` capped at 0.98 (never below `a`).
pub fn merged_confidence(a: f64, b: f64) -> f64 {
    (1.0 - (1.0 - a) * (1.0 - b)).min(MERGE_CONFIDENCE_CAP).max(a)
}

/// `2026-10-01` (midnight UTC) or an RFC 3339 timestamp.
pub fn parse_when(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?.and_hms_opt(0, 0, 0).map(|d| d.and_utc()))
}

/// Every UUID that appears in `text` (36-character hex-and-dash runs).
pub fn uuids_in(text: &str) -> Vec<Uuid> {
    text.split(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
        .filter(|t| t.len() == 36)
        .filter_map(|t| Uuid::parse_str(t).ok())
        .collect()
}

#[derive(Debug, sqlx::FromRow)]
struct FactRow {
    id: Uuid,
    scope_kind: ScopeKind,
    scope_key: String,
    subject_id: Uuid,
    predicate: String,
    status: FactStatus,
    pinned: bool,
    confidence: f64,
    confirmations: i32,
    evidence_observation_ids: Vec<Uuid>,
    learned_on_client_ids: Vec<Uuid>,
    first_seen_at: DateTime<Utc>,
    last_confirmed_at: DateTime<Utc>,
}

enum Decided {
    Applied,
    Rejected(String),
}

macro_rules! reject {
    ($($arg:tt)*) => { return Ok(Decided::Rejected(format!($($arg)*))) };
}

struct Run<'a> {
    packet: &'a PacketRecord,
    ctx: AuditCtx,
    /// `inputs.fact_ids`: the cluster. Empty = no restriction beyond the space.
    cluster: HashSet<Uuid>,
    touched: BTreeSet<(ScopeKind, String)>,
}

impl Run<'_> {
    /// A full UUID, or the 6-hex short id of a cluster member, as a cluster member.
    fn resolve(&self, id: &str) -> Option<Uuid> {
        let id = id.trim();
        if let Ok(uuid) = Uuid::parse_str(id) {
            return (self.cluster.is_empty() || self.cluster.contains(&uuid)).then_some(uuid);
        }
        let tail = id.strip_prefix("fact#").unwrap_or(id).to_ascii_lowercase();
        if tail.len() < 6 || !tail.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut hits = self.cluster.iter().filter(|u| u.simple().to_string().ends_with(&tail));
        let first = hits.next().copied()?;
        hits.next().is_none().then_some(first)
    }

    async fn load(&self, conn: &mut PgConnection, id: Uuid) -> Result<Option<FactRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, scope_kind, scope_key, subject_id, predicate, status, pinned, confidence::float8 AS confidence,
                    confirmations, evidence_observation_ids, learned_on_client_ids, first_seen_at,
                    last_confirmed_at
               FROM facts WHERE id = $1 AND space_id = $2 FOR UPDATE",
        )
        .bind(id)
        .bind(self.packet.space_id)
        .fetch_optional(conn)
        .await
    }

    fn touch(&mut self, row: &FactRow) {
        self.touched.insert((row.scope_kind, row.scope_key.clone()));
    }

    /// Another active fact on `row`'s `(scope, subject, predicate)` than `except`.
    async fn active_at_key(
        &self,
        conn: &mut PgConnection,
        kind: ScopeKind,
        key: &str,
        row: &FactRow,
        except: &[Uuid],
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM facts WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3
                               AND subject_id = $4 AND predicate = $5 AND status = 'active' AND id <> ALL($6))",
        )
        .bind(self.packet.space_id)
        .bind(kind)
        .bind(key)
        .bind(row.subject_id)
        .bind(&row.predicate)
        .bind(except)
        .fetch_one(conn)
        .await
    }

    async fn audit(
        &self,
        conn: &mut PgConnection,
        action: &str,
        id: Uuid,
        before: Option<serde_json::Value>,
        reason: &str,
    ) -> Result<(), sqlx::Error> {
        let after = snapshot(conn, "facts", id).await?;
        record(conn, &self.ctx, action, "facts", id, before, after, Some(reason)).await
    }

    async fn supersede(
        &mut self,
        conn: &mut PgConnection,
        target: &str,
        by: &str,
        valid_from: Option<&str>,
        reason: &str,
    ) -> Result<Decided, ApiError> {
        let (Some(old_id), Some(new_id)) = (self.resolve(target), self.resolve(by)) else {
            reject!("supersede: unknown id {target} / {by}");
        };
        if old_id == new_id {
            reject!("supersede: target and candidate are the same fact");
        }
        let (Some(old), Some(new)) = (self.load(conn, old_id).await?, self.load(conn, new_id).await?) else {
            reject!("supersede: fact not found in the packet's space");
        };
        if old.status != FactStatus::Active || new.status != FactStatus::Candidate {
            reject!("supersede: target must be active and the replacement a candidate");
        }
        if old.pinned {
            reject!("supersede: {old_id} is pinned");
        }
        let when = match valid_from {
            Some(s) => match parse_when(s) {
                Some(t) => Some(t),
                None => reject!("supersede: bad valid_from {s:?}"),
            },
            None => None,
        };
        if self.active_at_key(conn, new.scope_kind, &new.scope_key, &new, &[old_id]).await? {
            reject!("supersede: another active fact already holds the candidate's key");
        }
        let (before_old, before_new) = (snapshot(conn, "facts", old_id).await?, snapshot(conn, "facts", new_id).await?);
        sqlx::query(
            "UPDATE facts SET status = 'superseded', valid_to = coalesce($2, now()), superseded_by = $3, conflict = false
              WHERE id = $1",
        )
        .bind(old_id)
        .bind(when)
        .bind(new_id)
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "UPDATE facts SET status = 'active', supersedes_id = $2, conflict = false,
                              valid_from = coalesce($3, valid_from, now())
              WHERE id = $1",
        )
        .bind(new_id)
        .bind(old_id)
        .bind(when)
        .execute(&mut *conn)
        .await?;
        self.audit(conn, "fact.supersede", old_id, before_old, reason).await?;
        self.audit(conn, "fact.create", new_id, before_new, reason).await?;
        self.touch(&old);
        self.touch(&new);
        Ok(Decided::Applied)
    }

    async fn merge_into(
        &mut self,
        conn: &mut PgConnection,
        target: &str,
        candidate: &str,
        reason: &str,
    ) -> Result<Decided, ApiError> {
        let (Some(target_id), Some(cand_id)) = (self.resolve(target), self.resolve(candidate)) else {
            reject!("merge_into: unknown id {target} / {candidate}");
        };
        if target_id == cand_id {
            reject!("merge_into: target and candidate are the same fact");
        }
        let (Some(t), Some(c)) = (self.load(conn, target_id).await?, self.load(conn, cand_id).await?) else {
            reject!("merge_into: fact not found in the packet's space");
        };
        if !matches!(t.status, FactStatus::Active | FactStatus::Candidate) || c.status != FactStatus::Candidate {
            reject!("merge_into: target must be active or candidate and the merged fact a candidate");
        }
        let (before_t, before_c) = (snapshot(conn, "facts", target_id).await?, snapshot(conn, "facts", cand_id).await?);
        let confidence = if t.pinned { t.confidence } else { merged_confidence(t.confidence, c.confidence) };
        sqlx::query(
            "UPDATE facts SET
               evidence_observation_ids = (SELECT coalesce(array_agg(DISTINCT x), '{}') FROM unnest(evidence_observation_ids || $2::uuid[]) x),
               learned_on_client_ids = (SELECT coalesce(array_agg(DISTINCT x), '{}') FROM unnest(learned_on_client_ids || $3::uuid[]) x),
               confirmations = confirmations + $4,
               confidence = $5::float8::float4,
               last_confirmed_at = greatest(last_confirmed_at, $6),
               first_seen_at = least(first_seen_at, $7)
             WHERE id = $1",
        )
        .bind(target_id)
        .bind(&c.evidence_observation_ids)
        .bind(&c.learned_on_client_ids)
        .bind(c.confirmations)
        .bind(confidence)
        .bind(c.last_confirmed_at)
        .bind(c.first_seen_at)
        .execute(&mut *conn)
        .await?;
        sqlx::query("UPDATE facts SET status = 'superseded', superseded_by = $2, conflict = false WHERE id = $1")
            .bind(cand_id)
            .bind(target_id)
            .execute(&mut *conn)
            .await?;
        self.audit(conn, "fact.confirm", target_id, before_t, reason).await?;
        self.audit(conn, "fact.supersede", cand_id, before_c, reason).await?;
        self.touch(&t);
        self.touch(&c);
        Ok(Decided::Applied)
    }

    async fn keep_both(&mut self, conn: &mut PgConnection, ids: &[String], reason: &str) -> Result<Decided, ApiError> {
        let mut rows = Vec::new();
        for id in ids {
            let Some(uuid) = self.resolve(id) else { reject!("keep_both: unknown id {id}") };
            let Some(row) = self.load(conn, uuid).await? else { reject!("keep_both: {uuid} not found in the space") };
            if !rows.iter().any(|r: &FactRow| r.id == uuid) {
                rows.push(row);
            }
        }
        // Candidates become active side by side; their keys must not collide with each other or
        // with an existing active fact.
        let candidates: Vec<&FactRow> = rows.iter().filter(|r| r.status == FactStatus::Candidate).collect();
        let mut keys = HashSet::new();
        for c in &candidates {
            let key = (c.scope_kind, c.scope_key.clone(), c.subject_id, c.predicate.clone());
            if !keys.insert(key) || self.active_at_key(conn, c.scope_kind, &c.scope_key, c, &[]).await? {
                reject!("keep_both: {} would duplicate an active (scope, subject, predicate)", c.id);
            }
        }
        for c in candidates {
            let before = snapshot(conn, "facts", c.id).await?;
            sqlx::query(
                "UPDATE facts SET status = 'active', conflict = false, supersedes_id = NULL,
                                  valid_from = coalesce(valid_from, now())
                  WHERE id = $1",
            )
            .bind(c.id)
            .execute(&mut *conn)
            .await?;
            self.audit(conn, "fact.create", c.id, before, reason).await?;
        }
        for r in &rows {
            self.touch(r);
        }
        Ok(Decided::Applied)
    }

    async fn retract(&mut self, conn: &mut PgConnection, target: &str, reason: &str) -> Result<Decided, ApiError> {
        let Some(id) = self.resolve(target) else { reject!("retract: unknown id {target}") };
        let Some(row) = self.load(conn, id).await? else { reject!("retract: {id} not found in the space") };
        if !matches!(row.status, FactStatus::Active | FactStatus::Candidate) {
            reject!("retract: {id} is {}", row.status);
        }
        if row.pinned {
            reject!("retract: {id} is pinned");
        }
        let cited = uuids_in(reason);
        let evidence: i64 = if cited.is_empty() {
            0
        } else {
            sqlx::query_scalar("SELECT count(*) FROM observations WHERE space_id = $1 AND id = ANY($2)")
                .bind(self.packet.space_id)
                .bind(&cited)
                .fetch_one(&mut *conn)
                .await?
        };
        if evidence == 0 {
            reject!("retract: the reason for {id} cites no evidence observation id");
        }
        let before = snapshot(conn, "facts", id).await?;
        sqlx::query("UPDATE facts SET status = 'retracted', valid_to = now(), conflict = false WHERE id = $1")
            .bind(id)
            .execute(&mut *conn)
            .await?;
        self.audit(conn, "fact.retract", id, before, reason).await?;
        self.touch(&row);
        Ok(Decided::Applied)
    }

    async fn promote_scope(
        &mut self,
        conn: &mut PgConnection,
        target: &str,
        to: ScopeHint,
        reason: &str,
    ) -> Result<Decided, ApiError> {
        let Some(id) = self.resolve(target) else { reject!("promote_scope: unknown id {target}") };
        let Some(row) = self.load(conn, id).await? else { reject!("promote_scope: {id} not found in the space") };
        if !matches!(row.status, FactStatus::Active | FactStatus::Candidate) || row.pinned {
            reject!("promote_scope: {id} is {} / pinned={}", row.status, row.pinned);
        }
        if to != ScopeHint::Global {
            reject!("promote_scope: only promotion to global is supported, got {to}");
        }
        if row.scope_kind == ScopeKind::Global {
            reject!("promote_scope: {id} is already global");
        }
        if row.status == FactStatus::Active && self.active_at_key(conn, ScopeKind::Global, "", &row, &[id]).await? {
            reject!("promote_scope: an active global fact already holds ({}, {})", row.subject_id, row.predicate);
        }
        let before = snapshot(conn, "facts", id).await?;
        sqlx::query("UPDATE facts SET scope_kind = 'global', scope_key = '' WHERE id = $1")
            .bind(id)
            .execute(&mut *conn)
            .await?;
        sqlx::query(
            "UPDATE embeddings SET scope_kind = 'global', scope_key = '' WHERE owner_table = 'facts' AND owner_id = $1",
        )
        .bind(id)
        .execute(&mut *conn)
        .await?;
        self.audit(conn, "fact.promote", id, before, reason).await?;
        self.touch(&row);
        self.touched.insert((ScopeKind::Global, String::new()));
        Ok(Decided::Applied)
    }

    async fn set_confidence(
        &mut self,
        conn: &mut PgConnection,
        target: &str,
        confidence: f64,
        reason: &str,
    ) -> Result<Decided, ApiError> {
        let Some(id) = self.resolve(target) else { reject!("set_confidence: unknown id {target}") };
        let Some(row) = self.load(conn, id).await? else { reject!("set_confidence: {id} not found in the space") };
        if !matches!(row.status, FactStatus::Active | FactStatus::Candidate) || row.pinned {
            reject!("set_confidence: {id} is {} / pinned={}", row.status, row.pinned);
        }
        if !(0.0..=1.0).contains(&confidence) {
            reject!("set_confidence: {confidence} is outside 0..1");
        }
        let before = snapshot(conn, "facts", id).await?;
        sqlx::query("UPDATE facts SET confidence = $2::float8::float4 WHERE id = $1")
            .bind(id)
            .bind(confidence)
            .execute(&mut *conn)
            .await?;
        self.audit(conn, "fact.confirm", id, before, reason).await?;
        self.touch(&row);
        Ok(Decided::Applied)
    }
}

fn action_name(d: &FactDecision) -> &'static str {
    match d {
        FactDecision::Supersede { .. } => "supersede",
        FactDecision::MergeInto { .. } => "merge_into",
        FactDecision::KeepBoth { .. } => "keep_both",
        FactDecision::Retract { .. } => "retract",
        FactDecision::PromoteScope { .. } => "promote_scope",
        FactDecision::SetConfidence { .. } => "set_confidence",
    }
}

/// Apply every decision of `out` in order inside `conn`'s transaction.
pub async fn apply_decisions(
    conn: &mut PgConnection,
    packet: &PacketRecord,
    out: &ConsolidateFactsV1,
) -> Result<AppliedCounts, ApiError> {
    let cluster: HashSet<Uuid> = packet
        .inputs
        .get("fact_ids")
        .and_then(|v| v.as_array())
        .map(|ids| ids.iter().filter_map(|v| v.as_str()?.parse().ok()).collect())
        .unwrap_or_default();
    let mut run = Run { packet, ctx: packet_ctx(packet), cluster, touched: BTreeSet::new() };
    let mut counts = AppliedCounts::new();
    counts.insert("decisions".into(), i64::try_from(out.decisions.len()).unwrap_or(i64::MAX));
    for decision in &out.decisions {
        let reason = decision.reason();
        let decided = match decision {
            FactDecision::Supersede { target_id, by_candidate_id, valid_from, .. } => {
                run.supersede(conn, target_id, by_candidate_id, valid_from.as_deref(), reason).await?
            }
            FactDecision::MergeInto { target_id, candidate_id, .. } => {
                run.merge_into(conn, target_id, candidate_id, reason).await?
            }
            FactDecision::KeepBoth { ids, .. } => run.keep_both(conn, ids, reason).await?,
            FactDecision::Retract { target_id, .. } => run.retract(conn, target_id, reason).await?,
            FactDecision::PromoteScope { target_id, to, .. } => run.promote_scope(conn, target_id, *to, reason).await?,
            FactDecision::SetConfidence { target_id, confidence, .. } => {
                run.set_confidence(conn, target_id, *confidence, reason).await?
            }
        };
        let key = match decided {
            Decided::Applied => action_name(decision),
            Decided::Rejected(why) => {
                tracing::warn!(packet = %packet.id, action = action_name(decision), "consolidation decision rejected: {why}");
                "rejected"
            }
        };
        *counts.entry(key.into()).or_insert(0) += 1;
    }
    let space = packet.space_id;
    for (kind, key) in &run.touched {
        notify_invalidate(conn, space, *kind, key).await?;
        profiles::refresh_scope(conn, space, *kind, key).await?;
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_combination() {
        assert!((merged_confidence(0.6, 0.6) - 0.84).abs() < 1e-9);
        assert_eq!(merged_confidence(0.95, 0.95), MERGE_CONFIDENCE_CAP);
        assert_eq!(merged_confidence(0.99, 0.5), 0.99);
        assert_eq!(merged_confidence(0.0, 0.0), 0.0);
    }

    #[test]
    fn dates_and_uuids() {
        assert_eq!(parse_when("2026-10-01").unwrap().to_rfc3339(), "2026-10-01T00:00:00+00:00");
        assert!(parse_when("2026-10-01T05:00:00+02:00").is_some());
        assert!(parse_when("yesterday").is_none());
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert_eq!(uuids_in(&format!("contradicted by obs {a}, and {b}.")), vec![a, b]);
        assert!(uuids_in("no ids, only 1234-5678 and abc").is_empty());
    }
}
