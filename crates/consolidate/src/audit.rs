//! `memory_audit` rows (spec 4.6) and the row snapshots they carry.
//!
//! Every automated change records the full row `before` and `after` (generated columns left out)
//! and the run id, which is what [`crate::revert`] replays.

use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use memvault_core::ids::new_id;

/// Tables whose rows the audit trail snapshots and a revert can restore. All have `id uuid PRIMARY KEY`.
pub const AUDITED_TABLES: [&str; 7] =
    ["facts", "observations", "summaries", "procedures", "sessions", "work_state", "procedure_hints"];

/// Who is changing what, for the audit rows of one packet application or planner run.
#[derive(Debug, Clone)]
pub struct AuditCtx {
    pub space_id: Uuid,
    /// `packet:<id>` | `planner` | `user:<id>`.
    pub actor: String,
    pub run_id: Uuid,
    pub model: Option<String>,
    pub prompt_version: Option<String>,
}

impl AuditCtx {
    pub fn planner(space_id: Uuid, run_id: Uuid) -> Self {
        Self { space_id, actor: "planner".into(), run_id, model: None, prompt_version: None }
    }
}

/// The row as JSON without its generated columns, or `None` when it does not exist.
pub async fn snapshot(conn: &mut PgConnection, table: &str, id: Uuid) -> Result<Option<Value>, sqlx::Error> {
    assert!(AUDITED_TABLES.contains(&table), "table {table} is not audited");
    sqlx::query_scalar(&format!("SELECT to_jsonb(t) - 'search_text' - 'search_tsv' FROM {table} t WHERE id = $1"))
        .bind(id)
        .fetch_optional(conn)
        .await
}

/// Insert one audit row.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    conn: &mut PgConnection,
    ctx: &AuditCtx,
    action: &str,
    table: &str,
    target_id: Uuid,
    before: Option<Value>,
    after: Option<Value>,
    reason: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO memory_audit (id, space_id, actor, action, target_table, target_id, before, after, reason,
                                   model, prompt_version, run_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(new_id())
    .bind(ctx.space_id)
    .bind(&ctx.actor)
    .bind(action)
    .bind(table)
    .bind(target_id)
    .bind(before)
    .bind(after)
    .bind(reason)
    .bind(&ctx.model)
    .bind(&ctx.prompt_version)
    .bind(ctx.run_id)
    .execute(conn)
    .await?;
    Ok(())
}
