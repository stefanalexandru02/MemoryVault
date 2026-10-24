//! A Send-friendly equivalent of `memvault_core::user_tx`.
//!
//! The core version runs `sqlx::raw_sql(&format!(..))`, whose future rustc cannot prove `Send` for
//! every lifetime ("implementation of `Executor` is not general enough"), so an axum handler or MCP
//! tool that awaits it does not compile. This does the same with bound parameters (memvault-recall
//! keeps the same workaround). Drop it once core switches (requested).

use memvault_core::Identity;
use memvault_core::db::caps;
use sqlx::{PgPool, Postgres, Transaction};

/// Same contract as `memvault_core::user_tx`: role `memsvc_app` (switched only when the login is a
/// superuser), `app.user_id` and `app.client_id` set for the transaction.
pub async fn user_tx(pool: &PgPool, identity: &Identity) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let caps = caps(pool).await?;
    let mut tx = pool.begin().await?;
    if caps.superuser {
        sqlx::query("SET LOCAL ROLE memsvc_app").execute(&mut *tx).await?;
    }
    sqlx::query("SELECT set_config('app.user_id', $1, true), set_config('app.client_id', $2, true)")
        .bind(identity.user_id.to_string())
        .bind(identity.client_id.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
