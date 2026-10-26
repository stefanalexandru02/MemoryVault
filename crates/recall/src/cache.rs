//! Context cache (spec 11.6).
//!
//! `context_cache` holds the cached portion of a SessionStart block (profile, open work,
//! procedures, observation index, recent sessions) per key [`cache_key`]. Writers NOTIFY
//! `memory_invalidate` with `<space>:<scope_kind>:<scope_key>`; [`spawn_listener`] (started by
//! `memvault serve`) deletes the matching rows through the SECURITY DEFINER function
//! `memsvc_recall_invalidate` and queues one `jobs(type='render_context')` per deleted block, due
//! after `CONTEXT_RERENDER_DEBOUNCE_S` (2 s) and coalesced per key, for the worker to hand to
//! [`rerender`].

use std::time::Duration;

use chrono::{DateTime, Utc};
use memvault_core::invalidate::{CHANNEL, parse_payload};
use memvault_core::schemas::common::{InjectionMode, Scope};
use memvault_core::schemas::recall::DEFAULT_CONTEXT_ROWS;
use memvault_core::{ApiError, AppState, Identity};

use crate::context::ContextOptions;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgListener;
use sqlx::{PgConnection, PgPool};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Cached blocks older than this are re-rendered on read even without an invalidation (a lost
/// NOTIFY, or time-dependent ranking drifting).
pub const MAX_AGE_SECONDS: i64 = 3600;
/// Invalidated blocks rendered longer ago than this are not re-rendered by the worker.
const RERENDER_IF_USED_WITHIN_DAYS: i32 = 7;
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// What a block was rendered with besides who asked and from where: the injection mode and, in the full
/// block, the size and types of the observation index (a lean block is not a full one, nor are 6 rows 12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub mode: InjectionMode,
    pub rows: u32,
    /// Sorted, without duplicates; empty = every type.
    pub types: Vec<String>,
}

/// The 11.6 key `(spaces, sorted scopes, device, source)`, plus the user because rows are per user
/// (a render reflects that user's memberships) and the [`Variant`].
pub fn cache_key(
    user_id: Uuid,
    spaces: &[Uuid],
    scopes: &[Scope],
    device_id: &str,
    source: &str,
    variant: &Variant,
) -> String {
    let mut spaces = spaces.to_vec();
    spaces.sort();
    spaces.dedup();
    let mut scopes: Vec<String> = scopes.iter().map(Scope::as_text).collect();
    scopes.sort();
    scopes.dedup();
    let spaces: Vec<String> = spaces.iter().map(Uuid::to_string).collect();
    let Variant { mode, rows, types } = variant;
    let raw = format!(
        "v3\n{user_id}\n{}\n{}\n{device_id}\n{source}\n{mode}\n{rows}\n{}",
        spaces.join(","),
        scopes.join("\n"),
        types.join(",")
    );
    hex::encode(&Sha256::digest(raw.as_bytes())[..16])
}

/// Strong ETag (quoted) for a body.
pub fn etag(body: &str) -> String {
    format!("\"{}\"", hex::encode(&Sha256::digest(body.as_bytes())[..12]))
}

/// A cached block and the number of memories it was rendered from.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Cached {
    pub body: String,
    pub hits: i32,
}

/// The cached block for `key` if it is younger than [`MAX_AGE_SECONDS`] and no matching invalidation
/// happened after its render began (a render racing with a write). Runs on the user's
/// `user_tx` (rows are visible to their user only).
pub async fn lookup(conn: &mut PgConnection, key: &str) -> Result<Option<Cached>, sqlx::Error> {
    sqlx::query_as(
        "SELECT body_md AS body, hit_count AS hits FROM context_cache
          WHERE cache_key = $1 AND rendered_at > now() - make_interval(secs => $2)
            AND NOT memsvc_context_stale(space_ids, scopes, rendered_at)",
    )
    .bind(key)
    .bind(MAX_AGE_SECONDS as f64)
    .fetch_optional(conn)
    .await
}

/// What a cached block was rendered from.
#[derive(Debug, Clone)]
pub struct CacheEntry<'a> {
    pub key: &'a str,
    pub identity: &'a Identity,
    pub spaces: &'a [Uuid],
    pub scopes: &'a [Scope],
    pub project_key: Option<&'a str>,
    pub device_id: &'a str,
    pub source: &'a str,
    pub variant: &'a Variant,
    /// Memories the block was rendered from.
    pub hits: i32,
    /// When the render began (see [`render_start`]), not when it is stored.
    pub rendered_at: DateTime<Utc>,
}

/// Database clock reading to take before a render reads anything; stored as `rendered_at`.
pub async fn render_start(conn: &mut PgConnection) -> Result<DateTime<Utc>, sqlx::Error> {
    sqlx::query_scalar("SELECT clock_timestamp()").fetch_one(conn).await
}

/// Upsert a rendered block (on the user's `user_tx`).
pub async fn store(conn: &mut PgConnection, entry: &CacheEntry<'_>, body: &str) -> Result<(), sqlx::Error> {
    let scopes: Vec<String> = entry.scopes.iter().map(Scope::as_text).collect();
    sqlx::query(
        "INSERT INTO context_cache (cache_key, user_id, space_id, space_ids, scopes, device_id, source, body_md, etag,
                                    token_estimate, rendered_at, client_id, project_key, mode, hit_count, obs_rows, obs_types)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $13, $11, $12, $14, $15, $16, $17)
         ON CONFLICT (cache_key) DO UPDATE SET
           space_id = EXCLUDED.space_id, space_ids = EXCLUDED.space_ids, scopes = EXCLUDED.scopes,
           device_id = EXCLUDED.device_id, source = EXCLUDED.source, body_md = EXCLUDED.body_md,
           etag = EXCLUDED.etag, token_estimate = EXCLUDED.token_estimate, rendered_at = EXCLUDED.rendered_at,
           client_id = EXCLUDED.client_id, project_key = EXCLUDED.project_key, mode = EXCLUDED.mode,
           hit_count = EXCLUDED.hit_count, obs_rows = EXCLUDED.obs_rows, obs_types = EXCLUDED.obs_types",
    )
    .bind(entry.key)
    .bind(entry.identity.user_id)
    .bind(entry.spaces[0])
    .bind(entry.spaces.to_vec())
    .bind(scopes)
    .bind(entry.device_id)
    .bind(entry.source)
    .bind(body)
    .bind(etag(body))
    .bind(memvault_core::estimate_tokens(body))
    .bind(entry.identity.client_id)
    .bind(entry.project_key)
    .bind(entry.rendered_at)
    .bind(entry.variant.mode)
    .bind(entry.hits)
    .bind(i32::try_from(entry.variant.rows).unwrap_or(i32::MAX))
    .bind(&entry.variant.types)
    .execute(conn)
    .await?;
    Ok(())
}

/// Payload of a `jobs(type='render_context')` row: enough to call
/// [`crate::context::render_context`] again for the block's user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct RenderContextJob {
    pub cache_key: String,
    pub user_id: Uuid,
    pub client_id: Option<Uuid>,
    pub project_key: Option<String>,
    pub device_id: Option<String>,
    pub source: Option<String>,
    /// Absent in jobs queued before the injection modes existed.
    #[serde(default)]
    pub mode: InjectionMode,
    /// Index size and types of the block (absent in jobs queued before they were configurable).
    #[serde(default = "default_rows")]
    pub obs_rows: i32,
    #[serde(default)]
    pub obs_types: Vec<String>,
}

fn default_rows() -> i32 {
    i32::try_from(DEFAULT_CONTEXT_ROWS).unwrap_or(i32::MAX)
}

#[derive(sqlx::FromRow)]
struct Invalidated {
    #[sqlx(flatten)]
    job: RenderContextJob,
    recent: bool,
}

/// Delete the cached blocks matching an invalidation and queue their re-render. `space = None`
/// drops every block; `kind = None` a whole space. Runs on a plain pool connection (the SQL
/// function is SECURITY DEFINER). Returns the number of blocks deleted.
pub async fn invalidate(
    pool: &PgPool,
    space: Option<Uuid>,
    scope: Option<(&str, &str)>,
    debounce: Duration,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let deleted: Vec<Invalidated> = sqlx::query_as(
        "SELECT cache_key, user_id, client_id, project_key, device_id, source, mode, obs_rows, obs_types,
                rendered_at > now() - make_interval(days => $4) AS recent
           FROM memsvc_recall_invalidate($1, $2, $3)",
    )
    .bind(space)
    .bind(scope.map(|s| s.0))
    .bind(scope.map(|s| s.1))
    .bind(RERENDER_IF_USED_WITHIN_DAYS)
    .fetch_all(&mut *tx)
    .await?;
    for Invalidated { job, .. } in deleted.iter().filter(|d| d.recent && d.job.client_id.is_some()) {
        sqlx::query(
            "INSERT INTO jobs (id, type, payload, run_after)
             SELECT $1, 'render_context', $2, now() + make_interval(secs => $3)
              WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE type = 'render_context' AND status = 'queued'
                                                    AND payload->>'cache_key' = $4)",
        )
        .bind(memvault_core::ids::new_id())
        .bind(serde_json::to_value(job).expect("serializable"))
        .bind(debounce.as_secs_f64())
        .bind(&job.cache_key)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(deleted.len() as u64)
}

/// Apply one `memory_invalidate` payload (`<space>:<kind>:<key>`, or a bare space id).
pub async fn handle_payload(state: &AppState, payload: &str) -> Result<u64, sqlx::Error> {
    let debounce = state.settings.context_rerender_debounce;
    if let Some((space, kind, key)) = parse_payload(payload) {
        return invalidate(&state.db.app, Some(space), Some((kind.as_str(), &key)), debounce).await;
    }
    match payload.trim().parse::<Uuid>() {
        Ok(space) => invalidate(&state.db.app, Some(space), None, debounce).await,
        Err(_) => {
            tracing::warn!(payload, "ignoring malformed memory_invalidate payload");
            Ok(0)
        }
    }
}

async fn listen(state: &AppState) -> anyhow::Result<()> {
    let mut listener = PgListener::connect_with(&state.db.app).await?;
    listener.listen(CHANNEL).await?;
    // Notifications sent while nobody listened are lost: start from an empty cache.
    let dropped = invalidate(&state.db.app, None, None, state.settings.context_rerender_debounce).await?;
    tracing::info!(dropped, "context cache listener started");
    loop {
        let Some(n) = listener.try_recv().await? else {
            anyhow::bail!("listener connection lost");
        };
        match handle_payload(state, n.payload()).await {
            Ok(deleted) => tracing::debug!(payload = n.payload(), deleted, "context cache invalidated"),
            Err(e) => tracing::warn!(error = %e, "context cache invalidation failed"),
        }
    }
}

/// API-side `LISTEN memory_invalidate` loop deleting matching `context_cache` rows (spec 11.6).
/// Reconnects after errors (dropping the whole cache, since notifications may have been missed);
/// stops when `shutdown` is cancelled.
pub fn spawn_listener(state: AppState, shutdown: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                r = listen(&state) => if let Err(e) = r {
                    tracing::warn!(error = %e, "context cache listener stopped; reconnecting");
                },
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            }
        }
    })
}

/// Worker entry point for a `render_context` job: render the block again for its user so the next
/// SessionStart is a cache hit. Returns false when the user or client is gone or disabled (the job
/// is then done; nothing to render).
pub async fn rerender(state: &AppState, payload: &serde_json::Value) -> Result<bool, ApiError> {
    let job: RenderContextJob = serde_json::from_value(payload.clone())
        .map_err(|e| ApiError::invalid(format!("bad render_context payload: {e}")))?;
    let Some(client_id) = job.client_id else { return Ok(false) };
    type Row = (Option<Uuid>, String, String);
    let row: Option<Row> = sqlx::query_as(
        "SELECT u.default_space_id, c.device_id, c.kind FROM users u JOIN clients c ON c.user_id = u.id
          WHERE u.id = $1 AND c.id = $2 AND u.disabled_at IS NULL",
    )
    .bind(job.user_id)
    .bind(client_id)
    .fetch_optional(&state.db.system)
    .await?;
    let Some((default_space_id, device_id, client_kind)) = row else { return Ok(false) };
    let identity = Identity {
        user_id: job.user_id,
        client_id,
        scopes: Vec::new(),
        default_space_id,
        token_id: Uuid::nil(),
        token_kind: "system".into(),
        device_id,
        client_kind,
    };
    let options = ContextOptions {
        mode: Some(job.mode),
        rows: u32::try_from(job.obs_rows).ok(),
        types: job.obs_types.iter().filter_map(|t| t.parse().ok()).collect(),
    };
    crate::context::render_context_with(
        state,
        &identity,
        job.project_key.as_deref(),
        job.device_id.as_deref(),
        job.source.as_deref().unwrap_or("startup"),
        &options,
    )
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use memvault_core::schemas::common::ScopeKind;

    #[test]
    fn key_ignores_order_and_duplicates() {
        let (u, a, b) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let s1 = [Scope::global(), Scope::new(ScopeKind::Project, "p")];
        let s2 = [Scope::new(ScopeKind::Project, "p"), Scope::global(), Scope::global()];
        let key =
            |u, spaces: &[Uuid], scopes: &[Scope], source, v: &Variant| cache_key(u, spaces, scopes, "d", source, v);
        let full = Variant { mode: InjectionMode::Full, rows: 12, types: vec![] };
        assert_eq!(key(u, &[a, b], &s1, "startup", &full), key(u, &[b, a, a], &s2, "startup", &full));
        assert_ne!(key(u, &[a], &s1, "startup", &full), key(u, &[a], &s1, "clear", &full));
        assert_ne!(key(u, &[a], &s1, "startup", &full), key(a, &[a], &s1, "startup", &full));
        let other = |v: Variant| key(u, &[a], &s1, "startup", &v);
        assert_ne!(other(full.clone()), other(Variant { mode: InjectionMode::Lean, ..full.clone() }));
        assert_ne!(other(full.clone()), other(Variant { rows: 6, ..full.clone() }));
        assert_ne!(other(full.clone()), other(Variant { types: vec!["decision".into()], ..full.clone() }));
        assert_eq!(key(u, &[a], &s1, "startup", &full).len(), 32);
        assert_eq!(etag("x"), etag("x"));
        assert!(etag("x").starts_with('"'));
    }
}
