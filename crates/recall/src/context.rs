//! The SessionStart block (spec 6.3 `GET /v1/context`, 11.5-11.7).
//!
//! The cached portion (profile, open work, procedures, observation index, recent sessions) comes
//! from `context_cache` ([`crate::cache`]); the health banner and since-you-were-here are computed
//! live on every request and spliced in. The observation index is sized (`rows`) and limited (`types`) by the
//! request; both are part of the cache key. In `lean` mode the block is only the profile, open work
//! and a rubric for the pull tools ([`render::LEAN_RUBRIC`]), with no since-you-were-here; `smart` mode starts with
//! the same block and adds memories for the first prompt ([`crate::smart`]).

use std::time::Instant;

use chrono::{DateTime, Utc};
use memvault_core::auth::resolve_read_spaces;
use memvault_core::schemas::common::{InjectionMode, ObservationType, Scope, ScopeKind, Sensitivity};
use memvault_core::schemas::recall::{Budgets, DEFAULT_CONTEXT_ROWS, MAX_CONTEXT_ROWS};
use memvault_core::{ApiError, AppState, Identity, estimate_tokens};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::cache::{self, CacheEntry, Variant};
use crate::engine::{
    Pack, PackOptions, RetrievalRecord, build_pack, log_cached_retrieval, log_pack_access, log_retrieval,
    record_metrics,
};
use crate::render::{self, SinceRow};
use crate::settings;
use memvault_core::user_tx;

/// Small budgets for the full block (architecture 6.2): `/v1/recall` without a query and with
/// `expand_top_observations = 0`. The facts budget only carries pinned facts (shown in Profile). The
/// observation budget is a ceiling of [`INDEX_ROW_TOKENS`] per index row, not the thing that sizes the index.
fn context_budgets(rows: u32) -> Budgets {
    Budgets {
        profile: 300,
        facts: 150,
        procedures: 200,
        observations: rows * INDEX_ROW_TOKENS,
        summaries: 200,
        work_state: 150,
        documents: 0,
    }
}
/// Estimated tokens an index row may cost at most (a title is cut at 200 characters).
const INDEX_ROW_TOKENS: u32 = 60;
/// The lean block: a short profile, pinned facts and open work around the ~120-token rubric, ~360 tokens at most.
pub const LEAN_BUDGETS: Budgets =
    Budgets { profile: 120, facts: 60, procedures: 0, observations: 0, summaries: 0, work_state: 80, documents: 0 };
const SINCE_LIMIT: i64 = 8;
/// A session of this device started this recently is the one asking for context (the hook creates
/// it first), so it does not count as the previous visit.
const CURRENT_SESSION_SECONDS: f64 = 600.0;
const STALE_PACKET_HOURS: i32 = 24;

/// global + `project:<key>` + `device:<id>`.
pub fn context_scopes(project_key: Option<&str>, device_id: &str) -> Vec<Scope> {
    let mut scopes = vec![Scope::global()];
    if let Some(p) = project_key {
        scopes.push(Scope::new(ScopeKind::Project, p));
    }
    if !device_id.is_empty() {
        scopes.push(Scope::new(ScopeKind::Device, device_id));
    }
    scopes
}

/// What a block was served from, for `retrieval_log`.
enum Served {
    Cache { hits: i32 },
    Render(Box<Pack>),
}

/// What a context request asks for besides where it comes from.
#[derive(Debug, Clone, Default)]
pub struct ContextOptions {
    /// The request's choice; without one the user's default applies (full when they set none).
    pub mode: Option<InjectionMode>,
    /// Rows of the observation index of a full block (default [`DEFAULT_CONTEXT_ROWS`], at most
    /// [`MAX_CONTEXT_ROWS`]).
    pub rows: Option<u32>,
    /// Observation types the index is limited to; empty = all.
    pub types: Vec<ObservationType>,
}

/// A rendered block and the mode it was rendered in (None for a `resume`, which has no block).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlock {
    pub body: String,
    pub mode: Option<InjectionMode>,
}

/// [`render_context_with`] with the default index: the mode is the request's or the user's default.
pub async fn render_context(
    state: &AppState,
    identity: &Identity,
    project_key: Option<&str>,
    device_id: Option<&str>,
    source: &str,
    mode: Option<InjectionMode>,
) -> Result<String, ApiError> {
    let options = ContextOptions { mode, ..ContextOptions::default() };
    Ok(render_context_with(state, identity, project_key, device_id, source, &options).await?.body)
}

/// The SessionStart markdown block: profile, since-you-were-here (live), open work, procedures,
/// recent observation index with legend; or, in lean mode, profile, open work and the tool rubric.
/// `source = "resume"` returns an empty string.
pub async fn render_context_with(
    state: &AppState,
    identity: &Identity,
    project_key: Option<&str>,
    device_id: Option<&str>,
    source: &str,
    options: &ContextOptions,
) -> Result<ContextBlock, ApiError> {
    if source == "resume" {
        return Ok(ContextBlock { body: String::new(), mode: None });
    }
    let started = Instant::now();
    let project_key = project_key.map(str::trim).filter(|p| !p.is_empty());
    let device = device_id.map(str::trim).filter(|d| !d.is_empty()).unwrap_or(&identity.device_id);
    let mut tx = user_tx(&state.db.app, identity).await?;
    let spaces = resolve_read_spaces(&mut tx, identity, None).await?;
    let mode = match options.mode {
        Some(mode) => mode,
        None => settings::injection_mode(&mut tx).await?,
    };
    if spaces.is_empty() {
        return Ok(ContextBlock { body: render::assemble_context(&[], "", ""), mode: Some(mode) });
    }
    let variant = variant_of(mode, options);
    let budgets = match mode {
        InjectionMode::Full => context_budgets(variant.rows),
        InjectionMode::Lean | InjectionMode::Smart => LEAN_BUDGETS,
    };
    let scopes = context_scopes(project_key, device);
    let key = cache::cache_key(identity.user_id, &spaces, &scopes, device, source, &variant);
    let (body, served) = match cache::lookup(&mut tx, &key).await? {
        Some(cached) => {
            metrics::counter!("memsvc_context_cache_hit_total").increment(1);
            (cached.body, Served::Cache { hits: cached.hits })
        }
        None => {
            metrics::counter!("memsvc_context_cache_miss_total").increment(1);
            let rendered_at = cache::render_start(&mut tx).await?;
            let opts = PackOptions {
                query: None,
                entities: Vec::new(),
                intent: None,
                budgets,
                expand: 0,
                rerank: false,
                exclude_sensitivity: vec![Sensitivity::Secret],
                context: true,
                index_rows: variant.rows as usize,
                index_types: variant.types.clone(),
                as_of: None,
            };
            let pack = build_pack(state, &mut tx, &spaces, &scopes, &opts).await?;
            let body = match mode {
                InjectionMode::Full => render::context_body(&pack.slots),
                InjectionMode::Lean | InjectionMode::Smart => render::lean_body(&pack.slots),
            };
            let entry = CacheEntry {
                key: &key,
                identity,
                spaces: &spaces,
                scopes: &scopes,
                project_key,
                device_id: device,
                source,
                variant: &variant,
                hits: hits_of(&pack),
                rendered_at,
            };
            cache::store(&mut tx, &entry, &body).await?;
            (body, Served::Render(Box::new(pack)))
        }
    };
    let (since, listed) = match (mode, project_key) {
        (InjectionMode::Full, Some(p)) => since_you_were_here(&mut tx, identity, &spaces, p, device).await?,
        _ => (String::new(), Vec::new()),
    };
    let banners = health_banners(&mut tx, &spaces).await?;
    let out = render::assemble_context(&banners, &render::without_listed_sessions(&body, &listed), &since);
    let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let rec = |hits| RetrievalRecord {
        mode: "context",
        query: None,
        spaces: &spaces,
        scopes: &scopes,
        budgets: Some(&budgets),
        latency_ms,
        injection: Some(mode),
        tokens: estimate_tokens(&out),
        hits,
    };
    match &served {
        Served::Cache { hits } => {
            log_cached_retrieval(&mut tx, identity, rec(*hits)).await?;
        }
        Served::Render(pack) => {
            log_retrieval(&mut tx, identity, rec(hits_of(pack)), pack).await?;
            log_pack_access(&mut tx, identity, "context", &pack.slots).await?;
            record_metrics("context", pack, started.elapsed().as_secs_f64());
        }
    }
    tx.commit().await?;
    Ok(ContextBlock { body: out, mode: Some(mode) })
}

/// The index only exists in the full block: the other modes ignore its size and types, so they share a cache entry.
fn variant_of(mode: InjectionMode, options: &ContextOptions) -> Variant {
    if mode != InjectionMode::Full {
        return Variant { mode, rows: DEFAULT_CONTEXT_ROWS, types: Vec::new() };
    }
    let mut types: Vec<String> = options.types.iter().map(|t| t.as_str().to_string()).collect();
    types.sort();
    types.dedup();
    Variant { mode, rows: options.rows.unwrap_or(DEFAULT_CONTEXT_ROWS).clamp(1, MAX_CONTEXT_ROWS), types }
}

fn hits_of(pack: &Pack) -> i32 {
    i32::try_from(pack.selected.len()).unwrap_or(i32::MAX)
}

/// Spec 11.7: sessions on this project run by other clients since this client's previous session
/// here (default: the last 14 days), newest first, with their session summary (or, while a session
/// is still running, its latest prompt summary). Returns the section and the summaries it lists, which the
/// cached Recent sessions do not repeat.
pub async fn since_you_were_here(
    conn: &mut PgConnection,
    identity: &Identity,
    spaces: &[Uuid],
    project_key: &str,
    device: &str,
) -> Result<(String, Vec<Uuid>), sqlx::Error> {
    let last: Option<DateTime<Utc>> = sqlx::query_scalar(
        "WITH mine AS (
           SELECT id, started_at FROM sessions
            WHERE client_id = $1 AND project_key = $2 ORDER BY started_at DESC, id DESC),
         current AS (
           SELECT id FROM (SELECT * FROM mine LIMIT 1) m WHERE m.started_at > now() - make_interval(secs => $3))
         SELECT max(started_at) FROM mine WHERE id NOT IN (SELECT id FROM current)",
    )
    .bind(identity.client_id)
    .bind(project_key)
    .bind(CURRENT_SESSION_SECONDS)
    .fetch_one(&mut *conn)
    .await?;
    type Row = (DateTime<Utc>, String, String, Option<Uuid>, Option<String>, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT s.started_at, c.device_id, c.kind, sm.id, sm.request, sm.next_steps
           FROM sessions s JOIN clients c ON c.id = s.client_id
           LEFT JOIN LATERAL (
             SELECT id, request, next_steps FROM summaries
              WHERE session_id = s.id AND grain IN ('session', 'prompt')
              ORDER BY (grain = 'session') DESC, created_at DESC, id DESC LIMIT 1) sm ON true
          WHERE s.space_id = ANY($1) AND s.project_key = $2 AND s.client_id <> $3
            AND s.started_at > coalesce($4, now() - interval '14 days')
          ORDER BY s.started_at DESC, s.id DESC LIMIT $5",
    )
    .bind(spaces.to_vec())
    .bind(project_key)
    .bind(identity.client_id)
    .bind(last)
    .bind(SINCE_LIMIT)
    .fetch_all(conn)
    .await?;
    let rows: Vec<SinceRow> = rows
        .into_iter()
        .map(|(started_at, device_id, kind, summary_id, request, next_steps)| SinceRow {
            summary_id,
            started_at,
            device: if kind == "agentic-os" { "assistant".into() } else { device_id },
            request,
            next_steps,
        })
        .collect();
    let listed = rows.iter().filter_map(|r| r.summary_id).collect();
    Ok((render::since_section(device, last, &rows), listed))
}

/// Live banner lines: observer packets nobody has run for a day.
async fn health_banners(conn: &mut PgConnection, spaces: &[Uuid]) -> Result<Vec<String>, sqlx::Error> {
    let (waiting, oldest): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT count(*), min(created_at) FROM work_packets
          WHERE space_id = ANY($1) AND status IN ('pending', 'leased')
            AND created_at < now() - make_interval(hours => $2)",
    )
    .bind(spaces.to_vec())
    .bind(STALE_PACKET_HOURS)
    .fetch_one(conn)
    .await?;
    Ok(match oldest {
        Some(oldest) if waiting > 0 => vec![format!(
            "{waiting} work packet(s) waiting since {}; run `mem-hook observe --all`",
            render::fmt_date(Some(oldest))
        )],
        _ => Vec::new(),
    })
}
