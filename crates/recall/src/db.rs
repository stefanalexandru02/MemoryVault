//! Send-friendly equivalents of `memvault_core::user_tx` and `memvault_core::db::tune_vector_search`.
//!
//! The core versions run `sqlx::raw_sql(&format!(..))`, whose future rustc cannot prove `Send` for
//! every lifetime ("implementation of `Executor` is not general enough"), so an axum handler that
//! awaits them does not implement `Handler`. These do the same with bound parameters. Drop them once
//! core switches (requested).

use memvault_core::Identity;
use memvault_core::db::{DbCaps, caps};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

/// Same contract as `memvault_core::user_tx`: role `memsvc_app` (switched only when the login is a
/// superuser), `app.user_id` and `app.client_id` set for the transaction.
pub async fn user_tx(pool: &PgPool, identity: &Identity) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let caps = caps(pool).await?;
    let mut tx = pool.begin().await?;
    if caps.superuser {
        // memvault_core::db::APP_ROLE
        sqlx::query("SET LOCAL ROLE memsvc_app").execute(&mut *tx).await?;
    }
    sqlx::query("SELECT set_config('app.user_id', $1, true), set_config('app.client_id', $2, true)")
        .bind(identity.user_id.to_string())
        .bind(identity.client_id.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Same contract as `memvault_core::db::tune_vector_search`: `hnsw.ef_search`, plus
/// `hnsw.iterative_scan = relaxed_order` on pgvector >= 0.8, for the current transaction.
pub async fn tune_vector_search(conn: &mut PgConnection, caps: &DbCaps, ef_search: u32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config('hnsw.ef_search', $1, true),
                CASE WHEN $2 THEN set_config('hnsw.iterative_scan', 'relaxed_order', true) END",
    )
    .bind(ef_search.to_string())
    .bind(caps.hnsw_iterative_scan())
    .execute(conn)
    .await?;
    Ok(())
}
