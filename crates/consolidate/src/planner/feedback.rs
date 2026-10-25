//! Disputed facts (quality signals), in code only.
//!
//! A fact whose net negative feedback crossed the threshold carries `disputed_at` (set by
//! `memvault-memories`). It is retracted here under the run id, with the feedback ids as the reason,
//! so `revert_run` restores it. Pinned facts are never touched. A fact whose key has a candidate
//! waiting is left to the fact clusters, which supersede it. The queue mark is cleared before the
//! audited change, so a revert does not queue the fact again.

use chrono::{DateTime, Utc};
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::common::ScopeKind;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{AuditCtx, record, snapshot};

/// Disputed facts retracted per space and pass.
pub const MAX_PER_PASS: i64 = 100;
/// Feedback ids named in a reason.
const IDS_IN_REASON: usize = 5;

#[derive(Debug, sqlx::FromRow)]
struct Disputed {
    id: Uuid,
    scope_kind: ScopeKind,
    scope_key: String,
}

/// Retract the queued facts of the space; returns how many.
pub async fn retract_disputed(conn: &mut PgConnection, ctx: &AuditCtx, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let queued: Vec<Disputed> = sqlx::query_as(
        "SELECT f.id, f.scope_kind, f.scope_key FROM facts f
          WHERE f.space_id = $1 AND f.status = 'active' AND f.disputed_at IS NOT NULL AND NOT f.pinned
            AND NOT EXISTS (SELECT 1 FROM facts c
                             WHERE c.space_id = f.space_id AND c.scope_kind = f.scope_kind AND c.scope_key = f.scope_key
                               AND c.subject_id = f.subject_id AND c.predicate = f.predicate AND c.status = 'candidate')
          ORDER BY f.disputed_at, f.id LIMIT $2 FOR UPDATE OF f",
    )
    .bind(ctx.space_id)
    .bind(MAX_PER_PASS)
    .fetch_all(&mut *conn)
    .await?;
    for fact in &queued {
        let given: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT id, verdict FROM memory_feedback WHERE target_id = $1 ORDER BY created_at, id")
                .bind(fact.id)
                .fetch_all(&mut *conn)
                .await?;
        let reason = reason(&given);
        sqlx::query("UPDATE facts SET disputed_at = NULL WHERE id = $1").bind(fact.id).execute(&mut *conn).await?;
        let before = snapshot(conn, "facts", fact.id).await?;
        sqlx::query("UPDATE facts SET status = 'retracted', valid_to = $2, conflict = false WHERE id = $1")
            .bind(fact.id)
            .bind(now)
            .execute(&mut *conn)
            .await?;
        let after = snapshot(conn, "facts", fact.id).await?;
        record(conn, ctx, "fact.retract", "facts", fact.id, before, after, Some(&reason)).await?;
        notify_invalidate(conn, ctx.space_id, fact.scope_kind, &fact.scope_key).await?;
    }
    Ok(queued.len() as u64)
}

/// `disputed by feedback: 1 wrong, 1 outdated, 0 helpful (feedback <id>, ...)`.
fn reason(given: &[(Uuid, String)]) -> String {
    let count = |verdict: &str| given.iter().filter(|(_, v)| v == verdict).count();
    let ids: Vec<String> = given.iter().take(IDS_IN_REASON).map(|(id, _)| id.to_string()).collect();
    format!(
        "disputed by feedback: {} wrong, {} outdated, {} helpful (feedback {})",
        count("wrong"),
        count("outdated"),
        count("helpful"),
        ids.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_counts_verdicts_and_names_feedback() {
        let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let given = vec![(a, "wrong".to_string()), (b, "outdated".to_string()), (c, "helpful".to_string())];
        assert_eq!(
            reason(&given),
            format!("disputed by feedback: 1 wrong, 1 outdated, 1 helpful (feedback {a}, {b}, {c})")
        );
    }
}
