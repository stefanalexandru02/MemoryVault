//! `system_tx` with a `Send` future.
//!
//! Same behaviour as `memvault_core::db::system_tx`, whose future is not `Send` (its
//! `sqlx::raw_sql(..).execute(&mut *tx)` call defeats higher-ranked `Send` inference), so axum
//! handlers cannot await it. This one goes through `Executor::execute`. Drop this module once core
//! makes the same change.

use memvault_core::db::{SYSTEM_ROLE, caps};
use sqlx::{Executor, PgPool, Postgres, Transaction};

/// Transaction as `memsvc_system` (RLS bypass). Errors when the pool's login is neither a superuser
/// nor able to bypass RLS.
pub async fn system_tx(pool: &PgPool) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let caps = caps(pool).await?;
    if !caps.superuser && !caps.bypass_rls {
        return Err(sqlx::Error::Configuration(
            format!("system pool logs in as {} without BYPASSRLS; set DATABASE_SYSTEM_URL", caps.current_user).into(),
        ));
    }
    let mut tx = pool.begin().await?;
    if caps.superuser {
        let sql = format!("SET LOCAL ROLE {SYSTEM_ROLE}");
        (&mut *tx).execute(sqlx::raw_sql(&sql)).await?;
    }
    Ok(tx)
}
