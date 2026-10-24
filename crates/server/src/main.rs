//! `memvault`: serve, worker, planner, migrate, admin, evals (spec 2, 14).

use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use memvault_core::db::MIGRATOR;
use memvault_core::{AppState, Db, Settings};
use memvault_server::{all_appliers, build_router, shutdown_signal};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "memvault", version, about = "MemoryVault shared memory server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP API and MCP server.
    Serve {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
    },
    /// Run the worker loop (embed jobs, sweeps, re-render, retention).
    Worker,
    /// Run one consolidation planning pass.
    Planner {
        #[arg(long)]
        space_id: Option<uuid::Uuid>,
    },
    /// Apply pending SQL migrations (default: DATABASE_SYSTEM_URL, else DATABASE_URL).
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
            let addr = format!("{}:{}", host.unwrap_or_else(|| settings.host.clone()), port.unwrap_or(settings.port));
            let state = AppState::connect(settings, all_appliers()).await?;
            let handle = memvault_core::metrics::prometheus_handle();
            let shutdown = CancellationToken::new();
            let upkeep = {
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_secs(5));
                    while !shutdown.is_cancelled() {
                        tokio::select! {
                            _ = tick.tick() => handle.run_upkeep(),
                            () = shutdown.cancelled() => {},
                        }
                    }
                })
            };
            let listener = memvault_recall::cache::spawn_listener(state.clone(), shutdown.clone());
            let tcp = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
            tracing::info!(%addr, "memvault serving");
            axum::serve(tcp, build_router(state)).with_graceful_shutdown(shutdown_signal()).await?;
            shutdown.cancel();
            let _ = tokio::join!(upkeep, listener);
            Ok(())
        }
        Command::Worker => {
            let state = AppState::connect(settings, all_appliers()).await?;
            let shutdown = CancellationToken::new();
            let signal = {
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    shutdown_signal().await;
                    shutdown.cancel();
                })
            };
            let result = memvault_worker::run_worker(state, shutdown).await;
            signal.abort();
            result
        }
        Command::Planner { space_id } => {
            let state = AppState::connect(settings, all_appliers()).await?;
            let result = memvault_consolidate::planner::run_planner(&state, space_id).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Command::Migrate { dsn } => {
            let dsn = dsn.unwrap_or_else(|| settings.system_database_url().to_string());
            let pool = sqlx::PgPool::connect(&dsn).await.context("connecting for migrations")?;
            let before: i64 = applied_count(&pool).await;
            MIGRATOR.run(&pool).await.context("applying migrations")?;
            let after = applied_count(&pool).await;
            println!("applied {} migration(s); {} total", after - before, after);
            Ok(())
        }
        Command::Admin { args } => {
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

async fn applied_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success").fetch_one(pool).await.unwrap_or(0)
}
