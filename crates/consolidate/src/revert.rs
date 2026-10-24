//! Revert (spec 12.4): replay the `memory_audit` rows of a run in reverse, restoring each `before`
//! snapshot (rows that did not exist before are deleted), write a `revert` audit row per change
//! and cancel the run's packets (their idempotency keys are released, so the planner can plan the
//! same work again). Reverting a run twice undoes nothing twice: an audit row that
//! already has a `revert` row is skipped.

use std::collections::{BTreeSet, HashMap};

use memvault_core::ApiError;
use memvault_core::ids::new_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::admin::RevertResponse;
use memvault_core::schemas::common::ScopeKind;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{AUDITED_TABLES, snapshot};
use crate::profiles;

#[derive(Debug, sqlx::FromRow)]
struct AuditRow {
    id: Uuid,
    space_id: Uuid,
    action: String,
    target_table: String,
    target_id: Uuid,
    before: Option<Value>,
    after: Option<Value>,
}

fn scope_of(v: &Value) -> Option<(ScopeKind, String)> {
    Some((v.get("scope_kind")?.as_str()?.parse().ok()?, v.get("scope_key")?.as_str()?.to_string()))
}

/// Tables whose rows have a scope and embeddings carrying a copy of it.
fn has_embeddings(table: &str) -> bool {
    matches!(table, "facts" | "observations" | "summaries" | "procedures")
}

/// Writable columns of `table` (everything but `id` and generated columns).
async fn columns(conn: &mut PgConnection, table: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = $1 AND is_generated = 'NEVER' AND column_name <> 'id'
          ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(conn)
    .await
}

/// A fact restored to `active` must not collide with another active fact on its key
/// (`facts_one_active`): 409 naming the holder instead of a database error.
async fn check_one_active(conn: &mut PgConnection, id: Uuid, merged: &Value) -> Result<(), ApiError> {
    if merged["status"] != "active" {
        return Ok(());
    }
    let holder: Option<Uuid> = sqlx::query_scalar(
        "SELECT o.id FROM facts o
          WHERE o.id <> $1 AND o.status = 'active' AND o.space_id = ($2->>'space_id')::uuid
            AND o.scope_kind::text = $2->>'scope_kind' AND o.scope_key = $2->>'scope_key'
            AND o.subject_id = ($2->>'subject_id')::uuid AND o.predicate = $2->>'predicate'
          LIMIT 1",
    )
    .bind(id)
    .bind(merged)
    .fetch_optional(conn)
    .await?;
    match holder {
        Some(h) => Err(ApiError::conflict(format!(
            "cannot restore fact {id} to active: fact {h} holds its key now; revert the later change first"
        ))),
        None => Ok(()),
    }
}

/// Revert every audited change of `run_id`. `actor` is recorded on the `revert` rows
/// (`client:<id>` for the API, `cli` for the command line). 404 when the run is unknown.
pub async fn revert_run(conn: &mut PgConnection, run_id: Uuid, actor: &str) -> Result<RevertResponse, ApiError> {
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM memory_audit WHERE run_id = $1)
             OR EXISTS (SELECT 1 FROM work_packets WHERE inputs ->> 'run_id' = $1::text)",
    )
    .bind(run_id)
    .fetch_one(&mut *conn)
    .await?;
    if !known {
        return Err(ApiError::not_found(format!("no consolidation run {run_id}")));
    }
    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT a.id, a.space_id, a.action, a.target_table, a.target_id, a.before, a.after
           FROM memory_audit a
          WHERE a.run_id = $1 AND a.action <> 'revert'
            AND NOT EXISTS (SELECT 1 FROM memory_audit r WHERE r.reverts_audit_id = a.id)
          ORDER BY a.seq DESC FOR UPDATE",
    )
    .bind(run_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut column_cache: HashMap<String, Vec<String>> = HashMap::new();
    let mut touched: BTreeSet<(Uuid, ScopeKind, String)> = BTreeSet::new();
    let mut reverted = 0_i64;
    for row in rows {
        if !AUDITED_TABLES.contains(&row.target_table.as_str()) {
            tracing::warn!(table = %row.target_table, audit = %row.id, "revert: table is not restorable; skipped");
            continue;
        }
        let table = row.target_table.as_str();
        let current = snapshot(conn, table, row.target_id).await?;
        let restored = match &row.before {
            Some(before) => {
                let Some(cur) = current.as_ref() else {
                    continue; // deleted since; nothing to restore into
                };
                if !column_cache.contains_key(table) {
                    column_cache.insert(table.to_string(), columns(conn, table).await?);
                }
                // Only columns the audited change moved are put back, and only while they still hold
                // the audited value; later changes to the row (ended sessions, confirmations,
                // evidence) stay.
                let cols: Vec<&String> = column_cache[table]
                    .iter()
                    .filter(|c| {
                        let (b, a, n) = (
                            before.get(c.as_str()),
                            row.after.as_ref().and_then(|a| a.get(c.as_str())),
                            cur.get(c.as_str()),
                        );
                        match &row.after {
                            Some(_) => b != a && n == a,
                            None => b != n,
                        }
                    })
                    .collect();
                if cols.is_empty() {
                    tracing::warn!(audit = %row.id, "revert: nothing left to restore on {table} {}", row.target_id);
                    continue;
                }
                let mut merged = cur.clone();
                for c in &cols {
                    merged[c.as_str()] = before.get(c.as_str()).cloned().unwrap_or(Value::Null);
                }
                if table == "facts" {
                    check_one_active(conn, row.target_id, &merged).await?;
                }
                let sets: Vec<String> = cols.iter().map(|c| format!("\"{c}\" = r.\"{c}\"")).collect();
                sqlx::query(&format!(
                    "UPDATE {table} AS t SET {} FROM jsonb_populate_record(NULL::{table}, $2) AS r WHERE t.id = $1",
                    sets.join(", ")
                ))
                .bind(row.target_id)
                .bind(before)
                .execute(&mut *conn)
                .await?;
                if has_embeddings(table) {
                    if let Some((kind, key)) = scope_of(&merged) {
                        sqlx::query(
                            "UPDATE embeddings SET scope_kind = $3, scope_key = $4 WHERE owner_table = $1 AND owner_id = $2",
                        )
                        .bind(table)
                        .bind(row.target_id)
                        .bind(kind)
                        .bind(key)
                        .execute(&mut *conn)
                        .await?;
                    }
                }
                snapshot(conn, table, row.target_id).await?
            }
            None => {
                if current.is_none() {
                    continue;
                }
                sqlx::query(&format!("DELETE FROM {table} WHERE id = $1"))
                    .bind(row.target_id)
                    .execute(&mut *conn)
                    .await?;
                if has_embeddings(table) {
                    sqlx::query("DELETE FROM embeddings WHERE owner_table = $1 AND owner_id = $2")
                        .bind(table)
                        .bind(row.target_id)
                        .execute(&mut *conn)
                        .await?;
                }
                None
            }
        };
        sqlx::query(
            "INSERT INTO memory_audit (id, space_id, actor, action, target_table, target_id, before, after, reason,
                                       run_id, reverts_audit_id)
             VALUES ($1, $2, $3, 'revert', $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(new_id())
        .bind(row.space_id)
        .bind(actor)
        .bind(table)
        .bind(row.target_id)
        .bind(&current)
        .bind(&restored)
        .bind(format!("revert of {}", row.action))
        .bind(run_id)
        .bind(row.id)
        .execute(&mut *conn)
        .await?;
        for v in [row.before.as_ref(), row.after.as_ref(), current.as_ref()].into_iter().flatten() {
            if let Some((kind, key)) = scope_of(v) {
                touched.insert((row.space_id, kind, key));
            }
        }
        reverted += 1;
    }
    sqlx::query("UPDATE procedure_hints SET consumed_at = NULL, consumed_run_id = NULL WHERE consumed_run_id = $1")
        .bind(run_id)
        .execute(&mut *conn)
        .await?;
    let packets_cancelled = sqlx::query(
        "UPDATE work_packets SET status = 'cancelled', leased_by = NULL, lease_until = NULL,
                idempotency_key = idempotency_key || ':reverted:' || $1::text
          WHERE inputs ->> 'run_id' = $1::text AND status <> 'cancelled'",
    )
    .bind(run_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    for (space, kind, key) in touched {
        notify_invalidate(conn, space, kind, &key).await?;
        profiles::refresh_scope(conn, space, kind, &key).await?;
    }
    Ok(RevertResponse { run_id, reverted, packets_cancelled: i64::try_from(packets_cancelled).unwrap_or(i64::MAX) })
}
