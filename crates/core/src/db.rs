//! Postgres pools, RLS transactions and database capability detection.
//!
//! - API/MCP code runs inside [`user_tx`] so RLS applies (spec 4.7, 5.3): role `memsvc_app`,
//!   `app.user_id` / `app.client_id` set for the transaction.
//! - Broker, worker and planner code runs inside [`system_tx`] (role `memsvc_system`, BYPASSRLS).
//! - In tests both pools connect as the `postgres` superuser; the helpers then `SET LOCAL ROLE`
//!   so grants and policies are exercised exactly as in production.
//! - Pass `&mut *tx` (a `&mut PgConnection`) to helpers; commit with `tx.commit().await?`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::auth::Identity;
use crate::config::Settings;

pub const APP_ROLE: &str = "memsvc_app";
pub const SYSTEM_ROLE: &str = "memsvc_system";

/// Every file in the top-level `migrations/` directory, embedded at compile time.
pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// The process's pools. `system` is the same pool as `app` when `DATABASE_SYSTEM_URL` is unset.
#[derive(Debug, Clone)]
pub struct Db {
    pub app: PgPool,
    pub system: PgPool,
}

impl Db {
    /// Open both pools and check connectivity.
    pub async fn connect(settings: &Settings) -> Result<Self, sqlx::Error> {
        let options = PgPoolOptions::new().max_connections(settings.db_pool_max);
        let app = options.clone().connect(&settings.database_url).await?;
        let system = match &settings.database_system_url {
            Some(url) if url != &settings.database_url => options.connect(url).await?,
            _ => app.clone(),
        };
        Ok(Self { app, system })
    }

    /// Pools that connect on first use (CLI commands that may not need the database).
    pub fn connect_lazy(settings: &Settings) -> Result<Self, sqlx::Error> {
        let options = PgPoolOptions::new().max_connections(settings.db_pool_max);
        let app = options.clone().connect_lazy(&settings.database_url)?;
        let system = match &settings.database_system_url {
            Some(url) if url != &settings.database_url => options.connect_lazy(url)?,
            _ => app.clone(),
        };
        Ok(Self { app, system })
    }

    /// One pool for both roles (tests: `#[sqlx::test]` hands out a superuser pool).
    pub fn single(pool: PgPool) -> Self {
        Self { app: pool.clone(), system: pool }
    }
}

/// What the database behind a pool supports, detected once per pool (see [`caps`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbCaps {
    pub superuser: bool,
    pub bypass_rls: bool,
    pub current_user: String,
    /// ParadeDB BM25 available: use `@@@` / `paradedb.score`; else the tsvector fallback (11.2).
    pub pg_search: bool,
    pub pgvector_version: Option<(u32, u32, u32)>,
}

impl DbCaps {
    /// The `hnsw.iterative_scan` GUC exists only in pgvector >= 0.8.
    pub fn hnsw_iterative_scan(&self) -> bool {
        self.pgvector_version.is_some_and(|v| v >= (0, 8, 0))
    }
}

fn parse_version(v: &str) -> (u32, u32, u32) {
    let mut parts = v.split('.').map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>());
    let mut next = || parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (next(), next(), next())
}

fn caps_key(options: &PgConnectOptions) -> String {
    format!(
        "{}@{}:{}/{}",
        options.get_username(),
        options.get_host(),
        options.get_port(),
        options.get_database().unwrap_or_default()
    )
}

fn caps_cache() -> &'static Mutex<HashMap<String, DbCaps>> {
    static CACHE: OnceLock<Mutex<HashMap<String, DbCaps>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Capabilities of the database behind `pool`, queried once per (user, host, port, database).
pub async fn caps(pool: &PgPool) -> Result<DbCaps, sqlx::Error> {
    let key = caps_key(&pool.connect_options());
    if let Some(c) = caps_cache().lock().expect("caps cache poisoned").get(&key) {
        return Ok(c.clone());
    }
    let (superuser, bypass_rls, current_user, pg_search, vector): (bool, bool, String, bool, Option<String>) =
        sqlx::query_as(
            "SELECT r.rolsuper, r.rolbypassrls, current_user::text,
                    EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_search'),
                    (SELECT extversion FROM pg_extension WHERE extname = 'vector')
               FROM pg_roles r WHERE r.rolname = current_user",
        )
        .fetch_one(pool)
        .await?;
    let caps = DbCaps {
        superuser,
        bypass_rls,
        current_user,
        pg_search,
        pgvector_version: vector.as_deref().map(parse_version),
    };
    caps_cache().lock().expect("caps cache poisoned").insert(key, caps.clone());
    Ok(caps)
}

/// Transaction as `memsvc_app` with `app.user_id` / `app.client_id` set (spec 5.3). RLS policies
/// read `app_user_id()`; nothing outside the caller's spaces is visible.
///
/// Statements here are bound queries rather than `raw_sql(&format!(..))`: rustc cannot prove the
/// latter's future `Send` for every lifetime, and axum handlers need `Send` futures.
pub async fn user_tx(pool: &PgPool, identity: &Identity) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let caps = caps(pool).await?;
    let mut tx = pool.begin().await?;
    if caps.superuser {
        sqlx::query(SET_APP_ROLE).execute(&mut *tx).await?;
    }
    sqlx::query("SELECT set_config('app.user_id', $1, true), set_config('app.client_id', $2, true)")
        .bind(identity.user_id.to_string())
        .bind(identity.client_id.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

const SET_APP_ROLE: &str = "SET LOCAL ROLE memsvc_app";
const SET_SYSTEM_ROLE: &str = "SET LOCAL ROLE memsvc_system";

/// Transaction as `memsvc_system` (RLS bypass) for the broker, worker and planner. Errors when the
/// pool's login is neither a superuser nor able to bypass RLS (a misconfigured system URL would
/// otherwise silently see nothing).
pub async fn system_tx(pool: &PgPool) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let caps = caps(pool).await?;
    if !caps.superuser && !caps.bypass_rls {
        return Err(sqlx::Error::Configuration(
            format!("system pool logs in as {} without BYPASSRLS; set DATABASE_SYSTEM_URL", caps.current_user).into(),
        ));
    }
    let mut tx = pool.begin().await?;
    if caps.superuser {
        sqlx::query(SET_SYSTEM_ROLE).execute(&mut *tx).await?;
    }
    Ok(tx)
}

/// Per-transaction HNSW settings for a recall (spec 4.5): `hnsw.ef_search`, and
/// `hnsw.iterative_scan = relaxed_order` when pgvector >= 0.8. Call inside a transaction.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>(_: T) {}

    /// axum handlers and `tokio::spawn` need these futures to be `Send` with borrowed arguments.
    #[allow(dead_code)]
    fn transaction_futures_are_send(pool: &PgPool, identity: &Identity, conn: &mut PgConnection, caps: &DbCaps) {
        assert_send(async {
            let tx = user_tx(pool, identity).await?;
            tx.commit().await
        });
        assert_send(async {
            let tx = system_tx(pool).await?;
            tx.commit().await
        });
        assert_send(tune_vector_search(conn, caps, 80));
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.6.0"), (0, 6, 0));
        assert_eq!(parse_version("0.8.1-dev"), (0, 8, 1));
        let caps = DbCaps {
            superuser: true,
            bypass_rls: true,
            current_user: "postgres".into(),
            pg_search: false,
            pgvector_version: Some((0, 8, 0)),
        };
        assert!(caps.hnsw_iterative_scan());
        assert!(!DbCaps { pgvector_version: Some((0, 6, 0)), ..caps }.hnsw_iterative_scan());
    }
}
