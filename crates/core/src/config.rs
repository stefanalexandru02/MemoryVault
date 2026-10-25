//! Process settings from environment variables (no prefix: `DATABASE_URL`, `TEI_EMBED_URL`, ...).
//!
//! [`Settings::from_env`] reads every variable once at startup; unset variables keep the
//! [`Default`] value. Handlers read settings from `AppState::settings`.

use std::env;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Settings {
    // --- database (spec 4.7, 14) ---
    /// API/MCP pool, role `memsvc_app` (RLS applies).
    pub database_url: String,
    /// Broker/worker/planner/migrations pool, role `memsvc_system` (BYPASSRLS). `None`: the app
    /// URL is used and `system_tx` refuses to run unless that login can bypass RLS.
    pub database_system_url: Option<String>,
    pub db_pool_max: u32,

    // --- embeddings and rerank (spec 13) ---
    pub tei_embed_url: String,
    pub tei_rerank_url: String,
    /// Value stored in `embeddings.model`.
    pub embed_model: String,
    pub embed_dims: usize,
    pub rerank_model: String,
    /// Single-query embeds on the request path.
    pub tei_embed_timeout: Duration,
    /// Worker batches of up to `EMBED_BATCH_SIZE` texts.
    pub tei_embed_batch_timeout: Duration,
    /// Spec 11.3: 250 ms, then recall falls back to RRF.
    pub tei_rerank_timeout: Duration,
    /// `FAKE_TEI=1`: deterministic in-process [`crate::tei::FakeTei`] (tests, local dev).
    pub fake_tei: bool,

    // --- prompts and output schemas (spec 10.1, 9.4) ---
    pub prompts_dir: PathBuf,
    pub schemas_dir: PathBuf,

    // --- auth (spec 5) ---
    /// HMAC key for token secret hashes; must be set in production.
    pub token_pepper: String,
    pub rate_limit_events_per_min: i64,
    pub rate_limit_default_per_min: i64,

    // --- ingest / packets (spec 9) ---
    pub window_tool_uses: i32,
    pub window_idle_seconds: i64,
    pub lease_seconds_default: i64,
    pub preferred_runner_seconds: i64,

    // --- worker (spec 13, 14, 4.8) ---
    pub worker_sweep_interval: Duration,
    /// Where `memvault worker` serves `/metrics` and `/health`; `None`: `HOST:PORT`.
    pub worker_metrics_addr: Option<String>,
    pub embed_batch_size: usize,
    pub embed_concurrency: usize,
    pub embed_max_part_tokens: usize,
    pub context_rerender_debounce: Duration,
    pub retention_events_payload_days: i64,
    pub retention_packets_days: i64,
    pub retention_logs_days: i64,

    // --- document sources ---
    /// Largest `content` of one `POST /v1/sources`, in bytes.
    pub source_max_bytes: usize,

    // --- consolidation triggers ---
    /// New observations in a scope since it was last planned that queue a planner run; 0 turns the trigger off.
    pub consolidate_observation_threshold: i64,
    /// A scope is planned at most once per interval; the nightly pass skips scopes planned inside it.
    pub consolidate_min_interval: Duration,

    // --- HTTP server, logging ---
    pub host: String,
    pub port: u16,
    /// `tracing_subscriber::EnvFilter` directive, e.g. `info` or `memvault=debug,info`.
    pub log_level: String,
    /// `json` (default, spec 15) or `text`.
    pub log_format: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            database_url: "postgres://memsvc_app@127.0.0.1:5432/memsvc".into(),
            database_system_url: None,
            db_pool_max: 10,
            tei_embed_url: "http://tei-embed:80".into(),
            tei_rerank_url: "http://tei-rerank:80".into(),
            embed_model: "bge-m3".into(),
            embed_dims: 1024,
            rerank_model: "bge-reranker-v2-m3".into(),
            tei_embed_timeout: Duration::from_secs(2),
            tei_embed_batch_timeout: Duration::from_secs(60),
            tei_rerank_timeout: Duration::from_millis(250),
            fake_tei: false,
            prompts_dir: "prompts".into(),
            schemas_dir: "schemas".into(),
            token_pepper: String::new(),
            rate_limit_events_per_min: 600,
            rate_limit_default_per_min: 120,
            window_tool_uses: 8,
            window_idle_seconds: 90,
            lease_seconds_default: 600,
            preferred_runner_seconds: 600,
            worker_sweep_interval: Duration::from_secs(30),
            worker_metrics_addr: None,
            embed_batch_size: 64,
            embed_concurrency: 2,
            embed_max_part_tokens: 512,
            context_rerender_debounce: Duration::from_secs(2),
            retention_events_payload_days: 30,
            retention_packets_days: 90,
            retention_logs_days: 180,
            source_max_bytes: 400 * 1024,
            consolidate_observation_threshold: 25,
            consolidate_min_interval: Duration::from_secs(900),
            host: "0.0.0.0".into(),
            port: 8000,
            log_level: "info".into(),
            log_format: "json".into(),
        }
    }
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

fn parse<T: FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match var(name) {
        None => Ok(default),
        Some(v) => {
            v.parse().map_err(|e| anyhow::anyhow!("{e}")).with_context(|| format!("invalid value for {name}: {v:?}"))
        }
    }
}

fn parse_bool(name: &str, default: bool) -> Result<bool> {
    match var(name).map(|v| v.to_ascii_lowercase()) {
        None => Ok(default),
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => Ok(true),
        Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => Ok(false),
        Some(v) => anyhow::bail!("invalid boolean for {name}: {v:?}"),
    }
}

fn secs(name: &str, default: Duration) -> Result<Duration> {
    let s: f64 = parse(name, default.as_secs_f64())?;
    Duration::try_from_secs_f64(s).with_context(|| format!("invalid duration for {name}: {s}"))
}

impl Settings {
    /// Read the environment. Durations are given in (fractional) seconds, e.g. `TEI_RERANK_TIMEOUT_S=0.25`.
    pub fn from_env() -> Result<Self> {
        let d = Self::default();
        Ok(Self {
            database_url: var("DATABASE_URL").unwrap_or(d.database_url),
            database_system_url: var("DATABASE_SYSTEM_URL"),
            db_pool_max: parse("DB_POOL_MAX", d.db_pool_max)?,
            tei_embed_url: var("TEI_EMBED_URL").unwrap_or(d.tei_embed_url),
            tei_rerank_url: var("TEI_RERANK_URL").unwrap_or(d.tei_rerank_url),
            embed_model: var("EMBED_MODEL").unwrap_or(d.embed_model),
            embed_dims: parse("EMBED_DIMS", d.embed_dims)?,
            rerank_model: var("RERANK_MODEL").unwrap_or(d.rerank_model),
            tei_embed_timeout: secs("TEI_EMBED_TIMEOUT_S", d.tei_embed_timeout)?,
            tei_embed_batch_timeout: secs("TEI_EMBED_BATCH_TIMEOUT_S", d.tei_embed_batch_timeout)?,
            tei_rerank_timeout: secs("TEI_RERANK_TIMEOUT_S", d.tei_rerank_timeout)?,
            fake_tei: parse_bool("FAKE_TEI", d.fake_tei)?,
            prompts_dir: var("PROMPTS_DIR").map(PathBuf::from).unwrap_or(d.prompts_dir),
            schemas_dir: var("SCHEMAS_DIR").map(PathBuf::from).unwrap_or(d.schemas_dir),
            token_pepper: var("TOKEN_PEPPER").unwrap_or(d.token_pepper),
            rate_limit_events_per_min: parse("RATE_LIMIT_EVENTS_PER_MIN", d.rate_limit_events_per_min)?,
            rate_limit_default_per_min: parse("RATE_LIMIT_DEFAULT_PER_MIN", d.rate_limit_default_per_min)?,
            window_tool_uses: parse("WINDOW_TOOL_USES", d.window_tool_uses)?,
            window_idle_seconds: parse("WINDOW_IDLE_SECONDS", d.window_idle_seconds)?,
            lease_seconds_default: parse("LEASE_SECONDS_DEFAULT", d.lease_seconds_default)?,
            preferred_runner_seconds: parse("PREFERRED_RUNNER_SECONDS", d.preferred_runner_seconds)?,
            worker_sweep_interval: secs("WORKER_SWEEP_INTERVAL_S", d.worker_sweep_interval)?,
            worker_metrics_addr: var("WORKER_METRICS_ADDR"),
            embed_batch_size: parse("EMBED_BATCH_SIZE", d.embed_batch_size)?,
            embed_concurrency: parse("EMBED_CONCURRENCY", d.embed_concurrency)?,
            embed_max_part_tokens: parse("EMBED_MAX_PART_TOKENS", d.embed_max_part_tokens)?,
            context_rerender_debounce: secs("CONTEXT_RERENDER_DEBOUNCE_S", d.context_rerender_debounce)?,
            retention_events_payload_days: parse("RETENTION_EVENTS_PAYLOAD_DAYS", d.retention_events_payload_days)?,
            retention_packets_days: parse("RETENTION_PACKETS_DAYS", d.retention_packets_days)?,
            retention_logs_days: parse("RETENTION_LOGS_DAYS", d.retention_logs_days)?,
            source_max_bytes: parse("SOURCE_MAX_BYTES", d.source_max_bytes)?,
            consolidate_observation_threshold: parse(
                "CONSOLIDATE_OBSERVATION_THRESHOLD",
                d.consolidate_observation_threshold,
            )?,
            consolidate_min_interval: secs("CONSOLIDATE_MIN_INTERVAL_S", d.consolidate_min_interval)?,
            host: var("HOST").unwrap_or(d.host),
            port: parse("PORT", d.port)?,
            log_level: var("LOG_LEVEL").unwrap_or(d.log_level),
            log_format: var("LOG_FORMAT").unwrap_or(d.log_format),
        })
    }

    /// Refuse to start a process that hashes or checks tokens with an empty `TOKEN_PEPPER`: the HMAC
    /// key would be empty and every token hash guessable from the secret alone.
    pub fn require_token_pepper(&self) -> Result<()> {
        anyhow::ensure!(!self.token_pepper.trim().is_empty(), "TOKEN_PEPPER must be set to a non-empty secret");
        Ok(())
    }

    /// The URL for the system pool: `DATABASE_SYSTEM_URL`, else `DATABASE_URL`.
    pub fn system_database_url(&self) -> &str {
        self.database_system_url.as_deref().unwrap_or(&self.database_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_token_pepper_is_rejected() {
        assert!(Settings::default().require_token_pepper().is_err());
        let blank = Settings { token_pepper: "  ".into(), ..Settings::default() };
        assert!(blank.require_token_pepper().is_err());
        let set = Settings { token_pepper: "pepper".into(), ..Settings::default() };
        assert!(set.require_token_pepper().is_ok());
    }
}
