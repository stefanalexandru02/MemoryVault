//! `user_tx` / `system_tx` with `Send` futures.
//!
//! Same behaviour as `memvault_core::{user_tx, system_tx}`, whose futures are not `Send` (their
//! `sqlx::raw_sql(..).execute(&mut *tx)` call defeats higher-ranked `Send` inference), so axum
//! handlers cannot await them. These go through `Executor::execute` instead. Drop this module once
//! core makes the same change.

use memvault_core::Identity;
use memvault_core::db::{APP_ROLE, SYSTEM_ROLE, caps};
use sqlx::{Executor, PgPool, Postgres, Transaction};

/// Transaction as `memsvc_app` with `app.user_id` / `app.client_id` set (spec 5.3).
pub async fn user_tx(pool: &PgPool, identity: &Identity) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let caps = caps(pool).await?;
    let mut tx = pool.begin().await?;
    let role = if caps.superuser { format!("SET LOCAL ROLE {APP_ROLE}; ") } else { String::new() };
    // UUIDs are safe to interpolate; one round trip for role + settings.
    let sql = format!(
        "{role}SELECT set_config('app.user_id', '{}', true), set_config('app.client_id', '{}', true)",
        identity.user_id, identity.client_id
    );
    (&mut *tx).execute(sqlx::raw_sql(&sql)).await?;
    Ok(tx)
}

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
