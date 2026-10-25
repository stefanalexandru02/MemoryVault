//! Server-internal job queue rows (`jobs`, spec 4.6). The worker (memvault-worker) drains them.

use sqlx::PgConnection;
use uuid::Uuid;

use crate::ids::new_id;

/// Queue (re-)embedding of one row: `jobs(type='embed', payload={"owner_table", "owner_id"})`.
/// Call it in the transaction that writes the row. Re-exported as
/// `memvault_worker::embed::enqueue_embed`; ingest uses this path because it cannot depend on the
/// worker crate.
pub async fn enqueue_embed(conn: &mut PgConnection, owner_table: &str, owner_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO jobs (id, type, payload) VALUES ($1, 'embed', $2)")
        .bind(new_id())
        .bind(serde_json::json!({ "owner_table": owner_table, "owner_id": owner_id }))
        .execute(conn)
        .await?;
    Ok(())
}
