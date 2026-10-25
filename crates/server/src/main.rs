//! `memvault`: serve, worker, planner, migrate, admin, evals (spec 2, 14).
//!
//! `serve` applies pending migrations before it takes traffic; `worker` and `planner` wait for them.
//! `migrate` runs the same step by hand.

use anyhow::Context;
use clap::{Parser, Subcommand};
use memvault_core::db;
use memvault_core::{AppState, Db, Settings};
use memvault_server::{all_appliers, serve, shutdown_signal};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

/// How long the worker and the planner wait for the API to finish migrating.
const SCHEMA_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Parser)]
#[command(name = "memvault", version, about = "MemoryVault shared memory server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP API and MCP server, applying pending migrations first.
    Serve {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
    },
    /// Run the worker loop (embed jobs, sweeps, re-render, retention); waits for the API to migrate.
    Worker,
    /// Run one consolidation planning pass (waits for the schema, like the worker).
    Planner {
        #[arg(long)]
        space_id: Option<uuid::Uuid>,
    },
    /// Apply pending SQL migrations by hand: `serve` already does this at start-up (default: DATABASE_SYSTEM_URL, else DATABASE_URL).
    Migrate {
        #[arg(long)]
        dsn: Option<String>,
    },
    /// Admin commands (users, spaces, tokens, consolidation, ...): `memvault admin --help`.
    #[command(disable_help_flag = true)]
    Admin {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Evaluation harness: `memvault evals --help`.
    #[command(disable_help_flag = true)]
    Evals {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn init_tracing(settings: &Settings) {
    let filter = EnvFilter::try_new(&settings.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr);
    if settings.log_format == "json" {
        builder.json().flatten_event(true).init();
    } else {
        builder.init();
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let settings = Settings::from_env()?;
    init_tracing(&settings);

    match cli.command {
        Command::Serve { host, port } => {
            settings.require_token_pepper()?;
            let addr = format!("{}:{}", host.unwrap_or_else(|| settings.host.clone()), port.unwrap_or(settings.port));
            let state = AppState::connect(settings, all_appliers()).await?;
            let tcp = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
            tracing::info!(%addr, "memvault serving");
            serve(state, tcp, shutdown_signal()).await
        }
        Command::Worker => {
            let state = AppState::connect(settings, all_appliers()).await?;
            db::wait_for_schema(&state.db.system, SCHEMA_WAIT).await?;
            let shutdown = CancellationToken::new();
            let signal = {
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    shutdown_signal().await;
                    shutdown.cancel();
                })
            };
            let batch_tei = memvault_worker::batch_embedder(&state);
            let result = memvault_worker::run_worker_with(state, batch_tei, shutdown).await;
            signal.abort();
            result
        }
        Command::Planner { space_id } => {
            let state = AppState::connect(settings, all_appliers()).await?;
            db::wait_for_schema(&state.db.system, SCHEMA_WAIT).await?;
            let result = memvault_consolidate::planner::run_planner(&state, space_id).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Command::Migrate { dsn } => {
            let dsn = dsn.unwrap_or_else(|| settings.system_database_url().to_string());
            let pool = sqlx::PgPool::connect(&dsn).await.context("connecting for migrations")?;
            let before = db::applied_migrations(&pool).await;
            db::migrate(&pool).await.context("applying migrations")?;
            let after = db::applied_migrations(&pool).await;
            println!("applied {} migration(s); {after} total", after - before);
            Ok(())
        }
        Command::Admin { args } => {
            settings.require_token_pepper()?;
            let db = Db::connect_lazy(&settings)?;
            let state = AppState::new(settings, db, all_appliers())?;
            memvault_consolidate::admin_cli::run(state, args).await
        }
        Command::Evals { args } => {
            let db = Db::connect_lazy(&settings)?;
            let state = AppState::new(settings, db, all_appliers())?;
            memvault_evals::cli::run(state, args).await
        }
    }
}
