//! `GET /v1/admin/doctor` and `memvault admin doctor`: one-shot diagnosis of the server side.
//!
//! Every check reads on its own `system_tx` (admin sees across spaces, so RLS tables must not be counted
//! as a user) with a short statement timeout, and reports its own error: one broken query never hides
//! the other checks.

use memvault_core::schemas::admin::{DoctorCheck, DoctorResponse};
use memvault_core::schemas::common::EmbeddingOwnerTable;
use memvault_core::{AppState, db, system_tx};
use sqlx::{PgConnection, Postgres, Transaction};

/// Per-check statement timeout, so a slow count cannot hold the endpoint.
const STATEMENT_TIMEOUT_MS: i64 = 3000;
/// Rows counted per table when looking for missing embeddings; more is reported as "N+".
const BACKLOG_CAP: i64 = 10_000;
/// Embed jobs older than this (since they were due) mean the worker is behind (warn) or not running (fail).
const EMBED_LAG_WARN_S: f64 = 300.0;
const EMBED_LAG_FAIL_S: f64 = 3600.0;
/// Pending work packets older than this mean no runner is keeping up.
const PACKET_AGE_WARN_S: f64 = 1800.0;
const PACKET_AGE_FAIL_S: f64 = 86_400.0;
/// A consolidation older than this is reported (the planner runs daily).
const CONSOLIDATION_WARN_S: f64 = 3.0 * 86_400.0;

const RUNNER_FIX: &str =
    "run `mem-hook observe --all` on a machine with `claude` installed (README: Nightly consolidation runner)";
const WORKER_FIX: &str = "start the worker: `memvault worker` (docker compose: `docker compose up -d worker`)";

/// Runs every check; never fails as a whole.
pub async fn doctor(state: &AppState) -> DoctorResponse {
    let mut checks = vec![guard("database", database(state).await)];
    checks.extend(extensions(state).await);
    checks.push(guard("embedding backlog", embedding_backlog(state).await));
    checks.push(guard("embed queue", embed_queue(state).await));
    checks.push(embedder(state).await);
    checks.push(guard("work packet queue", packet_queue(state).await));
    checks.push(guard("runners", runners(state).await));
    checks.push(guard("last consolidation", last_consolidation(state).await));
    DoctorResponse { checks }
}

fn guard(name: &str, result: Result<DoctorCheck, sqlx::Error>) -> DoctorCheck {
    result.unwrap_or_else(|e| DoctorCheck::fail(name, format!("query failed: {e}"), "check the database logs"))
}

async fn begin(state: &AppState) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let mut tx = system_tx(&state.db.system).await?;
    sqlx::query(&format!("SET LOCAL statement_timeout = {STATEMENT_TIMEOUT_MS}")).execute(&mut *tx).await?;
    Ok(tx)
}

/// `90s`, `12m`, `5h`, `3d`.
fn age(seconds: f64) -> String {
    let s = seconds.max(0.0);
    match s {
        s if s < 120.0 => format!("{s:.0}s"),
        s if s < 7200.0 => format!("{:.0}m", s / 60.0),
        s if s < 172_800.0 => format!("{:.0}h", s / 3600.0),
        s => format!("{:.0}d", s / 86_400.0),
    }
}

async fn database(state: &AppState) -> Result<DoctorCheck, sqlx::Error> {
    let mut tx = begin(state).await?;
    let (applied, failed): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE success), count(*) FILTER (WHERE NOT success) FROM _sqlx_migrations",
    )
    .fetch_one(&mut *tx)
    .await?;
    let known = i64::try_from(db::MIGRATOR.migrations.len()).unwrap_or(i64::MAX);
    Ok(if failed > 0 {
        DoctorCheck::fail(
            "database",
            format!("{failed} migration(s) failed ({applied} of {known} applied)"),
            "fix the failed migration by hand, then restart `memvault serve` (or run `memvault migrate`)",
        )
    } else if applied < known {
        DoctorCheck::fail(
            "database",
            format!("{applied} of {known} migrations applied"),
            "run `memvault migrate` (`memvault serve` does it at start-up)",
        )
    } else {
        DoctorCheck::ok("database", format!("reachable, {applied} migrations applied"))
    })
}

async fn extensions(state: &AppState) -> Vec<DoctorCheck> {
    let caps = match db::caps(&state.db.system).await {
        Ok(caps) => caps,
        Err(e) => {
            return vec![DoctorCheck::fail("extensions", format!("query failed: {e}"), "check the database logs")];
        }
    };
    let vector = match caps.pgvector_version {
        Some((major, minor, patch)) => DoctorCheck::ok("pgvector", format!("{major}.{minor}.{patch}")),
        None => DoctorCheck::fail(
            "pgvector",
            "extension missing: vector search cannot work",
            "use a Postgres image with pgvector and run `CREATE EXTENSION vector;`",
        ),
    };
    let search = if caps.pg_search {
        DoctorCheck::ok("pg_search", "BM25 keyword search")
    } else {
        DoctorCheck::warn(
            "pg_search",
            "extension missing: keyword search uses the tsvector fallback",
            "use the ParadeDB Postgres image (deploy/docker/postgres.Dockerfile) for BM25",
        )
    };
    vec![vector, search]
}

async fn embedding_backlog(state: &AppState) -> Result<DoctorCheck, sqlx::Error> {
    const NAME: &str = "embedding backlog";
    let mut tx = begin(state).await?;
    let mut missing = 0_i64;
    for table in [
        EmbeddingOwnerTable::Observations,
        EmbeddingOwnerTable::Summaries,
        EmbeddingOwnerTable::Facts,
        EmbeddingOwnerTable::Procedures,
        EmbeddingOwnerTable::Documents,
    ] {
        missing += sqlx::query_scalar::<_, i64>(&format!(
            "SELECT count(*) FROM (
                SELECT 1 FROM {table} t
                 WHERE {live} AND NOT EXISTS (
                       SELECT 1 FROM embeddings e WHERE e.owner_table = '{table}' AND e.owner_id = t.id AND e.model = $1)
                 LIMIT {BACKLOG_CAP}) missing",
            live = crate::ops::live_filter(table),
        ))
        .bind(&state.settings.embed_model)
        .fetch_one(&mut *tx)
        .await?;
    }
    if missing == 0 {
        return Ok(DoctorCheck::ok(NAME, format!("every row has a {} embedding", state.settings.embed_model)));
    }
    let queued = queued_embed_jobs(&mut tx).await?.0;
    let more = if missing >= BACKLOG_CAP { "+" } else { "" };
    let detail = format!(
        "{missing}{more} row(s) without a {} embedding, {queued} embed job(s) queued",
        state.settings.embed_model
    );
    Ok(if queued > 0 {
        DoctorCheck::ok(NAME, detail)
    } else {
        DoctorCheck::warn(NAME, detail, "queue them with `memvault admin reindex`")
    })
}

/// Queued embed jobs that are due, and how long the oldest has been due (seconds).
async fn queued_embed_jobs(conn: &mut PgConnection) -> Result<(i64, f64), sqlx::Error> {
    sqlx::query_as(
        "SELECT count(*), coalesce(extract(epoch FROM now() - min(run_after)), 0)::float8
           FROM jobs WHERE type = 'embed' AND status = 'queued' AND run_after <= now()",
    )
    .fetch_one(conn)
    .await
}

async fn embed_queue(state: &AppState) -> Result<DoctorCheck, sqlx::Error> {
    const NAME: &str = "embed queue";
    let mut tx = begin(state).await?;
    let (queued, oldest) = queued_embed_jobs(&mut tx).await?;
    if queued == 0 {
        return Ok(DoctorCheck::ok(NAME, "no embed jobs waiting"));
    }
    let detail = format!("{queued} embed job(s) waiting, the oldest for {}", age(oldest));
    Ok(if oldest >= EMBED_LAG_FAIL_S {
        DoctorCheck::fail(NAME, detail, WORKER_FIX)
    } else if oldest >= EMBED_LAG_WARN_S {
        DoctorCheck::warn(NAME, detail, WORKER_FIX)
    } else {
        DoctorCheck::ok(NAME, detail)
    })
}

async fn embedder(state: &AppState) -> DoctorCheck {
    const NAME: &str = "embedder";
    if state.settings.fake_tei {
        return DoctorCheck::warn(
            NAME,
            "FAKE_TEI is set: embeddings are hashed bags of words, search quality is not meaningful",
            "unset FAKE_TEI and set TEI_EMBED_URL / TEI_RERANK_URL",
        );
    }
    let health = state.tei.health().await;
    if !health.embed {
        DoctorCheck::fail(
            NAME,
            format!("embedding service ({}) is not answering", state.settings.tei_embed_url),
            "start the TEI embed service or fix TEI_EMBED_URL",
        )
    } else if !health.rerank {
        DoctorCheck::warn(
            NAME,
            format!(
                "rerank service ({}) is not answering: results fall back to fusion order",
                state.settings.tei_rerank_url
            ),
            "start the TEI rerank service or fix TEI_RERANK_URL",
        )
    } else {
        DoctorCheck::ok(NAME, format!("{} up, rerank up", state.settings.embed_model))
    }
}

async fn packet_queue(state: &AppState) -> Result<DoctorCheck, sqlx::Error> {
    const NAME: &str = "work packet queue";
    let mut tx = begin(state).await?;
    let (pending, leased, oldest): (i64, i64, f64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'pending'),
                count(*) FILTER (WHERE status = 'leased'),
                coalesce(extract(epoch FROM now() - min(available_after) FILTER (WHERE status = 'pending' AND available_after <= now())), 0)::float8
           FROM work_packets WHERE status IN ('pending', 'leased')",
    )
    .fetch_one(&mut *tx)
    .await?;
    if pending == 0 {
        return Ok(DoctorCheck::ok(NAME, format!("nothing pending, {leased} leased")));
    }
    let detail = format!("{pending} pending (oldest {}), {leased} leased", age(oldest));
    Ok(if oldest >= PACKET_AGE_FAIL_S {
        DoctorCheck::fail(NAME, detail, RUNNER_FIX)
    } else if oldest >= PACKET_AGE_WARN_S {
        DoctorCheck::warn(NAME, detail, RUNNER_FIX)
    } else {
        DoctorCheck::ok(NAME, detail)
    })
}

/// Whether `claude -p` runners are alive, judged by the newest lease: a packet's lease start is its
/// expiry minus its length. Only matters while packets wait.
async fn runners(state: &AppState) -> Result<DoctorCheck, sqlx::Error> {
    const NAME: &str = "runners";
    let mut tx = begin(state).await?;
    let (pending, last_lease): (i64, Option<f64>) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM work_packets WHERE status = 'pending'),
                (SELECT extract(epoch FROM now() - max(lease_until - make_interval(secs => coalesce(lease_seconds, $1::float8))))::float8
                   FROM work_packets WHERE leased_by IS NOT NULL)",
    )
    .bind(state.settings.lease_seconds_default as f64)
    .fetch_one(&mut *tx)
    .await?;
    Ok(match last_lease {
        Some(seconds) if seconds < PACKET_AGE_WARN_S || pending == 0 => {
            DoctorCheck::ok(NAME, format!("last packet leased {} ago", age(seconds)))
        }
        Some(seconds) => {
            DoctorCheck::warn(NAME, format!("last packet leased {} ago while {pending} wait", age(seconds)), RUNNER_FIX)
        }
        None if pending == 0 => DoctorCheck::ok(NAME, "no packet has been leased yet"),
        None => DoctorCheck::warn(NAME, format!("no packet has ever been leased, {pending} wait"), RUNNER_FIX),
    })
}

async fn last_consolidation(state: &AppState) -> Result<DoctorCheck, sqlx::Error> {
    const NAME: &str = "last consolidation";
    let mut tx = begin(state).await?;
    let last: Option<f64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM now() - max(created_at))::float8 FROM work_packets WHERE kind LIKE 'consolidate\\_%'",
    )
    .fetch_one(&mut *tx)
    .await?;
    Ok(match last {
        Some(seconds) if seconds < CONSOLIDATION_WARN_S => {
            DoctorCheck::ok(NAME, format!("packets planned {} ago", age(seconds)))
        }
        Some(seconds) => DoctorCheck::warn(
            NAME,
            format!("last planned {} ago", age(seconds)),
            "run `memvault admin consolidate now` and check the planner schedule",
        ),
        None => DoctorCheck::warn(NAME, "no consolidation packet exists", "run `memvault admin consolidate now`"),
    })
}
