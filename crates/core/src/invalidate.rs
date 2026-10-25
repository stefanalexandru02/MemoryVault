//! Context-cache invalidation signal (spec 11.6).
//!
//! Every write path that changes what a SessionStart block or recall would show calls
//! [`notify_invalidate`] inside its transaction. Postgres delivers the NOTIFY on commit to every
//! listener on channel [`CHANNEL`] with payload `<space_id>:<scope_kind>:<scope_key>`. The API's
//! listener (memvault-recall) deletes matching `context_cache` rows; the worker re-renders them.

use sqlx::PgConnection;
use uuid::Uuid;

use crate::schemas::common::ScopeKind;

pub const CHANNEL: &str = "memory_invalidate";

pub fn format_payload(space_id: Uuid, scope_kind: ScopeKind, scope_key: &str) -> String {
    format!("{space_id}:{}:{scope_key}", scope_kind.as_str())
}

/// Inverse of [`format_payload`]. `scope_key` may itself contain ':'.
pub fn parse_payload(payload: &str) -> Option<(Uuid, ScopeKind, String)> {
    let mut parts = payload.splitn(3, ':');
    let space = parts.next()?.parse().ok()?;
    let kind = parts.next()?.parse().ok()?;
    let key = parts.next()?.to_string();
    Some((space, kind, key))
}

/// `pg_notify('memory_invalidate', '<space>:<kind>:<key>')`, delivered when the transaction commits.
pub async fn notify_invalidate(
    conn: &mut PgConnection,
    space_id: Uuid,
    scope_kind: ScopeKind,
    scope_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(format_payload(space_id, scope_kind, scope_key))
        .execute(conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrip() {
        let id = crate::ids::new_id();
        let p = format_payload(id, ScopeKind::Project, "github.com/a/b:c");
        assert_eq!(parse_payload(&p), Some((id, ScopeKind::Project, "github.com/a/b:c".to_string())));
        assert_eq!(parse_payload("x:y"), None);
    }
}
