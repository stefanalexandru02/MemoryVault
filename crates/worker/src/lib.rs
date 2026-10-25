//! memvault-worker: embedding parts and jobs, worker loop, sweeps, retention (spec 4.8, 13, 14).

use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use chrono::{DateTime, Utc};
use memvault_consolidate::planner::{JobOutcome, run_plan_job};
use memvault_core::AppState;
use memvault_core::tei::{Embedder, TeiClient};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub mod embed;
pub mod parts;
pub mod queue;
pub mod retention;
pub mod sweep;

use embed::EmbedTarget;
use queue::Job;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const RENDER_JOB_TYPES: &[&str] = &["render_context"];
const BULK_JOB_TYPES: &[&str] = &["reindex", "retention"];
/// Planner runs have a lane of their own so a long reindex cannot hold them up.
const PLAN_JOB_TYPES: &[&str] = &[memvault_core::jobs::PLAN_JOB];

/// Embedder for worker batches: same model as `state.tei` but with `TEI_EMBED_BATCH_TIMEOUT_S`
/// instead of the 2 s single-query timeout. With `FAKE_TEI` it is `state.tei` itself.
pub fn batch_embedder(state: &AppState) -> Arc<dyn Embedder> {
    let settings = &state.settings;
    if settings.fake_tei {
        return state.tei.clone();
    }
    Arc::new(TeiClient::new(
        &settings.tei_embed_url,
        &settings.tei_rerank_url,
        &settings.embed_model,
        settings.embed_dims,
        settings.tei_embed_batch_timeout,
        settings.tei_rerank_timeout,
    ))
}

/// Claim and run one batch of embed jobs (up to `EMBED_BATCH_SIZE` rows). Returns the number of
/// jobs claimed.
pub async fn embed_pass(state: &AppState, worker: &str) -> anyhow::Result<usize> {
    embed_pass_with(state, &state.tei, worker).await
}

/// [`embed_pass`] with an explicit embedder. A failing batch is split in halves until the
/// failing jobs are alone, so one poison row cannot sink the others.
pub async fn embed_pass_with(state: &AppState, tei: &Arc<dyn Embedder>, worker: &str) -> anyhow::Result<usize> {
    let pool = &state.db.system;
    let jobs = queue::claim(pool, worker, &["embed"], state.settings.embed_batch_size.max(1) as i64).await?;
    if jobs.is_empty() {
        return Ok(0);
    }
    let ids: Vec<Uuid> = jobs.iter().map(|j| j.id).collect();
    queue::with_heartbeat(pool, &ids, run_embed_jobs(state, tei, &jobs)).await?;
    Ok(jobs.len())
}

async fn run_embed_jobs(state: &AppState, tei: &Arc<dyn Embedder>, jobs: &[Job]) -> anyhow::Result<()> {
    let pool = &state.db.system;
    let model = tei.model();
    let mut targets = Vec::new();
    let mut runnable = Vec::new();
    for job in jobs {
        match EmbedTarget::from_payload(&job.payload) {
            Ok(t) if t.model.as_deref().is_some_and(|m| m != model) => {
                let msg = format!("job is for model {}, this worker embeds {model}", t.model.unwrap_or_default());
                queue::fail(pool, job, &msg, true).await?;
                record_job("embed", "failed");
            }
            Ok(t) => {
                targets.push(t);
                runnable.push(job);
            }
            Err(e) => {
                queue::fail(pool, job, &format!("{e:#}"), true).await?;
                record_job("embed", "failed");
            }
        }
    }
    let mut stack = vec![(targets.as_slice(), runnable.as_slice())];
    while let Some((targets, runnable)) = stack.pop() {
        if targets.is_empty() {
            continue;
        }
        let started = std::time::Instant::now();
        match embed::embed_targets_with(state, &**tei, targets).await {
            Ok(parts) => {
                for job in runnable {
                    queue::complete(pool, job.id).await?;
                    record_job("embed", "done");
                }
                metrics::histogram!("memsvc_embed_batch_seconds").record(started.elapsed().as_secs_f64());
                tracing::debug!(jobs = runnable.len(), parts, "embedded");
            }
            Err(e) if runnable.len() > 1 => {
                tracing::warn!(error = %e, jobs = runnable.len(), "embed batch failed, splitting");
                let mid = runnable.len() / 2;
                stack.push((&targets[mid..], &runnable[mid..]));
                stack.push((&targets[..mid], &runnable[..mid]));
            }
            Err(e) => {
                tracing::warn!(error = %e, job = %runnable[0].id, "embed job failed");
                queue::fail(pool, runnable[0], &format!("{e:#}"), false).await?;
                record_job("embed", "retry");
            }
        }
    }
    Ok(())
}

/// Claim and run one job of the given types, renewing its lock while it runs.
pub async fn job_pass(state: &AppState, worker: &str, types: &[&str]) -> anyhow::Result<usize> {
    let pool = &state.db.system;
    let jobs = queue::claim(pool, worker, types, 1).await?;
    for job in &jobs {
        let kind = job.kind.as_str();
        match queue::with_heartbeat(pool, &[job.id], run_other(state, job)).await {
            Ok(JobEnd::Done) => {
                queue::complete(pool, job.id).await?;
                record_job(kind, "done");
            }
            Ok(JobEnd::Deferred(until)) => {
                queue::defer(pool, job.id, until).await?;
                record_job(kind, "deferred");
            }
            Err(e) => {
                tracing::warn!(job = %job.id, kind, error = %e, "job failed");
                queue::fail(pool, job, &format!("{e:#}"), false).await?;
                record_job(kind, "retry");
            }
        }
    }
    Ok(jobs.len())
}

/// How a job that ran without error ends.
enum JobEnd {
    Done,
    /// Not due yet: queue it again to run at this time.
    Deferred(DateTime<Utc>),
}

async fn run_other(state: &AppState, job: &Job) -> anyhow::Result<JobEnd> {
    match job.kind.as_str() {
        "render_context" => {
            memvault_recall::cache::rerender(state, &job.payload).await?;
        }
        "reindex" => {
            let model = job.payload.get("model").and_then(|m| m.as_str()).unwrap_or(state.tei.model());
            let queued = embed::reindex(&state.db.system, model).await?;
            tracing::info!(model, queued, "reindex queued");
        }
        "retention" => {
            let report = retention::run_retention(&state.db.system, &state.settings).await?;
            tracing::info!(?report, "retention done");
        }
        "plan" => {
            if let JobOutcome::Deferred(until) = run_plan_job(state, &job.payload, Utc::now()).await? {
                return Ok(JobEnd::Deferred(until));
            }
        }
        other => anyhow::bail!("unknown job type {other}"),
    }
    Ok(JobEnd::Done)
}

fn record_job(kind: &str, outcome: &'static str) {
    metrics::counter!("memsvc_jobs_total", "type" => kind.to_string(), "outcome" => outcome).increment(1);
}

enum Lane {
    Embed(Arc<dyn Embedder>),
    Jobs(&'static [&'static str]),
}

async fn job_loop(state: AppState, name: String, lane: Lane, shutdown: CancellationToken) {
    loop {
        let claimed = match &lane {
            Lane::Embed(tei) => embed_pass_with(&state, tei, &name).await,
            Lane::Jobs(types) => job_pass(&state, &name, types).await,
        };
        let idle = match claimed {
            Ok(n) => n == 0,
            Err(e) => {
                tracing::warn!(worker = name, error = %e, "job pass failed");
                true
            }
        };
        if idle {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        } else if shutdown.is_cancelled() {
            return;
        }
    }
}

async fn sweep_loop(state: AppState, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(state.settings.worker_sweep_interval);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => sweep::sweep(&state).await,
        }
    }
}

/// Serve `/metrics` and `/health` on `listener` until `shutdown`; runs recorder upkeep.
pub async fn serve_metrics(listener: tokio::net::TcpListener, shutdown: CancellationToken) {
    let handle = memvault_core::metrics::prometheus_handle();
    let app = Router::new()
        .route("/metrics", get(move || async move { handle.render() }))
        .route("/health", get(|| async { "ok" }));
    let upkeep = {
        let shutdown = shutdown.clone();
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = tick.tick() => handle.run_upkeep(),
                    _ = shutdown.cancelled() => return,
                }
            }
        }
    };
    let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let (served, ()) = tokio::join!(serve.into_future(), upkeep);
    if let Err(e) = served {
        tracing::error!(error = %e, "metrics server failed");
    }
}

/// The `memvault worker` loop with embeds going through `state.tei`. See [`run_worker_with`].
pub async fn run_worker(state: AppState, shutdown: CancellationToken) -> anyhow::Result<()> {
    let tei = state.tei.clone();
    run_worker_with(state, tei, shutdown).await
}

/// The `memvault worker` loop: `EMBED_CONCURRENCY` embed workers using `batch_tei`, one lane for
/// `render_context` (so the 2 s debounce is not held up), one for `reindex` and `retention`, one for
/// scoped `plan` jobs, the
/// sweep every `WORKER_SWEEP_INTERVAL_S` (idle windows, expired leases, retention scheduling,
/// gauges) and `/metrics` + `/health` on `WORKER_METRICS_ADDR` (default `HOST:PORT`). Returns once
/// `shutdown` is cancelled and the in-flight jobs finished.
pub async fn run_worker_with(
    state: AppState,
    batch_tei: Arc<dyn Embedder>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let id = format!("worker-{}", memvault_core::ids::short_id(&memvault_core::ids::new_id()));
    tracing::info!(worker = id, "worker started");
    let mut tasks = tokio::task::JoinSet::new();
    let addr = state
        .settings
        .worker_metrics_addr
        .clone()
        .unwrap_or_else(|| format!("{}:{}", state.settings.host, state.settings.port));
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => {
            tracing::info!(%addr, "worker metrics listening");
            tasks.spawn(serve_metrics(listener, shutdown.clone()));
        }
        Err(e) => tracing::error!(%addr, error = %e, "cannot bind worker metrics listener; metrics unavailable"),
    }
    for i in 0..state.settings.embed_concurrency.max(1) {
        let lane = Lane::Embed(batch_tei.clone());
        tasks.spawn(job_loop(state.clone(), format!("{id}-embed{i}"), lane, shutdown.clone()));
    }
    tasks.spawn(job_loop(state.clone(), format!("{id}-render"), Lane::Jobs(RENDER_JOB_TYPES), shutdown.clone()));
    tasks.spawn(job_loop(state.clone(), format!("{id}-bulk"), Lane::Jobs(BULK_JOB_TYPES), shutdown.clone()));
    tasks.spawn(job_loop(state.clone(), format!("{id}-plan"), Lane::Jobs(PLAN_JOB_TYPES), shutdown.clone()));
    tasks.spawn(sweep_loop(state.clone(), shutdown.clone()));
    while let Some(r) = tasks.join_next().await {
        if let Err(e) = r {
            tracing::error!(error = %e, "worker task panicked");
        }
    }
    tracing::info!("worker stopped");
    Ok(())
}
