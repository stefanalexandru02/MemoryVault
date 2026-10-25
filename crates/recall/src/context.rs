//! The SessionStart block (spec 6.3 `GET /v1/context`, 11.5-11.7).
//!
//! The cached portion (profile, open work, procedures, observation index, recent sessions) comes
//! from `context_cache` ([`crate::cache`]); the health banner and since-you-were-here are computed
//! live on every request and spliced in.

use std::time::Instant;

use chrono::{DateTime, Utc};
use memvault_core::auth::resolve_read_spaces;
use memvault_core::schemas::common::{Scope, ScopeKind, Sensitivity};
use memvault_core::schemas::recall::Budgets;
use memvault_core::{ApiError, AppState, Identity};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::cache::{self, CacheEntry};
use crate::engine::{PackOptions, RetrievalRecord, build_pack, log_pack_access, log_retrieval, record_metrics};
use crate::render::{self, SinceRow};
use memvault_core::user_tx;

/// Small budgets for a ~1k-token block (architecture 6.2): `/v1/recall` without a query and with
/// `expand_top_observations = 0`. The facts budget only carries pinned facts (shown in Profile).
pub const CONTEXT_BUDGETS: Budgets =
    Budgets { profile: 300, facts: 150, procedures: 200, observations: 700, summaries: 200, work_state: 150 };
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

/// The SessionStart markdown block: profile, since-you-were-here (live), open work, procedures,
/// recent observation index with legend. `source = "resume"` returns an empty string.
pub async fn render_context(
    state: &AppState,
    identity: &Identity,
    project_key: Option<&str>,
    device_id: Option<&str>,
    source: &str,
) -> Result<String, ApiError> {
    if source == "resume" {
        return Ok(String::new());
    }
    let started = Instant::now();
    let project_key = project_key.map(str::trim).filter(|p| !p.is_empty());
    let device = device_id.map(str::trim).filter(|d| !d.is_empty()).unwrap_or(&identity.device_id);
    let mut tx = user_tx(&state.db.app, identity).await?;
    let spaces = resolve_read_spaces(&mut tx, identity, None).await?;
    if spaces.is_empty() {
        return Ok(render::assemble_context(&[], "", ""));
    }
    let scopes = context_scopes(project_key, device);
    let key = cache::cache_key(identity.user_id, &spaces, &scopes, device, source);
    let body = match cache::lookup(&mut tx, &key).await? {
        Some(body) => {
            metrics::counter!("memsvc_context_cache_hit_total").increment(1);
            body
        }
        None => {
            metrics::counter!("memsvc_context_cache_miss_total").increment(1);
            let rendered_at = cache::render_start(&mut tx).await?;
            let opts = PackOptions {
                query: None,
                entities: Vec::new(),
                intent: None,
                budgets: CONTEXT_BUDGETS,
                expand: 0,
                rerank: false,
                exclude_sensitivity: vec![Sensitivity::Secret],
                context: true,
            };
            let pack = build_pack(state, &mut tx, &spaces, &scopes, &opts).await?;
            let body = render::context_body(&pack.slots);
            let entry = CacheEntry {
                key: &key,
                identity,
                spaces: &spaces,
                scopes: &scopes,
                project_key,
                device_id: device,
                source,
                rendered_at,
            };
            cache::store(&mut tx, &entry, &body).await?;
            let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
            let rec = RetrievalRecord {
                mode: "context",
                query: None,
                spaces: &spaces,
                scopes: &scopes,
                budgets: Some(&CONTEXT_BUDGETS),
                latency_ms,
            };
            log_retrieval(&mut tx, identity, rec, &pack).await?;
            log_pack_access(&mut tx, identity, "context", &pack.slots).await?;
            record_metrics("context", &pack, started.elapsed().as_secs_f64());
            body
        }
    };
    let since = match project_key {
        Some(p) => since_you_were_here(&mut tx, identity, &spaces, p, device).await?,
        None => String::new(),
    };
    let banners = health_banners(&mut tx, &spaces).await?;
    tx.commit().await?;
    Ok(render::assemble_context(&banners, &body, &since))
}

/// Spec 11.7: sessions on this project run by other clients since this client's previous session
/// here (default: the last 14 days), newest first, with their session summary (or, while a session
/// is still running, its latest prompt summary).
pub async fn since_you_were_here(
    conn: &mut PgConnection,
    identity: &Identity,
    spaces: &[Uuid],
    project_key: &str,
    device: &str,
) -> Result<String, sqlx::Error> {
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
    type Row = (DateTime<Utc>, String, String, Option<String>, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT s.started_at, c.device_id, c.kind, sm.request, sm.next_steps
           FROM sessions s JOIN clients c ON c.id = s.client_id
           LEFT JOIN LATERAL (
             SELECT request, next_steps FROM summaries
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
        .map(|(started_at, device_id, kind, request, next_steps)| SinceRow {
            started_at,
            device: if kind == "agentic-os" { "assistant".into() } else { device_id },
            request,
            next_steps,
        })
        .collect();
    Ok(render::since_section(device, last, &rows))
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
