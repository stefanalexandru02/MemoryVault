//! `memvault admin <args...>` (spec 5.2, 6.7): the admin API as a command line for `kubectl exec`.
//! Runs on the system pool; results are printed as JSON. `token create` prints the raw token once.
//!
//! ```text
//! memvault admin user create alex --display-name "Alex"
//! memvault admin space create team --owner alex
//! memvault admin membership add --space team --user alex --role writer
//! memvault admin token create --user alex --client laptop-x1 --kind device
//! memvault admin token revoke <id|prefix>
//! memvault admin consolidate now [--space-id <uuid>]
//! memvault admin consolidate revert <run_id>
//! memvault admin packets list --status failed | packets retry <id>
//! memvault admin reindex [--model m] [--table facts]... [--space-id <uuid>]
//! memvault admin project remap <from> <to> [--space-id <uuid>]
//! ```

use std::io::Write;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use memvault_core::AppState;
use memvault_core::auth::TokenKind;
use memvault_core::schemas::admin::{MembershipCreate, ReindexRequest, SpaceCreate, TokenCreate, UserCreate};
use memvault_core::schemas::common::{
    ClientKind, EmbeddingOwnerTable, MembershipRole, PacketKind, PacketStatus, SpaceKind,
};
use serde::Serialize;
use uuid::Uuid;

use crate::ops::{self, RemapRequest};
use crate::planner::run_planner;
use crate::revert::revert_run;
use crate::tx::system_tx;

/// Actor recorded on audit rows written from the command line.
const CLI_ACTOR: &str = "admin-cli";

#[derive(Debug, Parser)]
#[command(name = "memvault admin", about = "MemoryVault administration (users, spaces, tokens, consolidation)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Users.
    #[command(subcommand)]
    User(UserCommand),
    /// Spaces.
    #[command(subcommand)]
    Space(SpaceCommand),
    /// Space memberships.
    #[command(subcommand)]
    Membership(MembershipCommand),
    /// API tokens.
    #[command(subcommand)]
    Token(TokenCommand),
    /// Consolidation runs.
    #[command(subcommand)]
    Consolidate(ConsolidateCommand),
    /// Work packets.
    #[command(subcommand)]
    Packets(PacketsCommand),
    /// Server-internal jobs.
    #[command(subcommand)]
    Jobs(JobsCommand),
    /// Queue embedding jobs for existing rows.
    Reindex {
        /// Embedding model to index with (default: EMBED_MODEL).
        #[arg(long)]
        model: Option<String>,
        /// Table to reindex (repeatable; default: all).
        #[arg(long = "table", value_parser = parse_table)]
        tables: Vec<EmbeddingOwnerTable>,
        #[arg(long)]
        space_id: Option<Uuid>,
    },
    /// Project keys.
    #[command(subcommand)]
    Project(ProjectCommand),
}

#[derive(Debug, Subcommand)]
enum UserCommand {
    /// Create a user with a private default space.
    Create {
        handle: String,
        #[arg(long)]
        display_name: Option<String>,
    },
    List,
}

#[derive(Debug, Subcommand)]
enum SpaceCommand {
    Create {
        name: String,
        /// Owner: handle or id.
        #[arg(long)]
        owner: String,
        #[arg(long, default_value = "shared", value_parser = parse_space_kind)]
        kind: SpaceKind,
    },
    List,
}

#[derive(Debug, Subcommand)]
enum MembershipCommand {
    Add {
        /// Space id or exact name.
        #[arg(long)]
        space: String,
        /// User handle or id.
        #[arg(long)]
        user: String,
        #[arg(long, value_parser = parse_role)]
        role: MembershipRole,
    },
    List {
        #[arg(long)]
        space: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Mint a token; the raw value is printed once.
    Create {
        /// User handle or id.
        #[arg(long)]
        user: String,
        /// Device id of the client the token belongs to (created when missing).
        #[arg(long)]
        client: String,
        #[arg(long, default_value = "device", value_parser = parse_token_kind)]
        kind: TokenKind,
        #[arg(long)]
        client_kind: Option<String>,
        #[arg(long)]
        hostname: Option<String>,
        #[arg(long)]
        platform: Option<String>,
        /// Lifetime in days (default by kind: device 365, runner 90, admin 30).
        #[arg(long)]
        expires_days: Option<i64>,
    },
    /// Revoke by token id or 8-character prefix.
    Revoke { token: String },
    List {
        /// User handle or id.
        #[arg(long)]
        user: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConsolidateCommand {
    /// Run the planner now.
    Now {
        #[arg(long)]
        space_id: Option<Uuid>,
    },
    /// Undo a run from its audit rows.
    Revert { run_id: Uuid },
}

#[derive(Debug, Subcommand)]
enum PacketsCommand {
    List {
        #[arg(long, value_parser = parse_packet_status)]
        status: Option<PacketStatus>,
        #[arg(long, value_parser = parse_packet_kind)]
        kind: Option<PacketKind>,
        #[arg(long)]
        space_id: Option<Uuid>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Put a failed packet back in the queue.
    Retry {
        id: Uuid,
        /// Keep the attempt count (one extra attempt is granted).
        #[arg(long)]
        keep_attempts: bool,
    },
}

#[derive(Debug, Subcommand)]
enum JobsCommand {
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long = "type")]
        job_type: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Rename a project key everywhere (audited, revertable).
    Remap {
        from: String,
        to: String,
        #[arg(long)]
        space_id: Option<Uuid>,
    },
}

fn parse_table(s: &str) -> Result<EmbeddingOwnerTable, String> {
    s.parse().map_err(|e| format!("{e}"))
}
fn parse_space_kind(s: &str) -> Result<SpaceKind, String> {
    s.parse().map_err(|e| format!("{e}"))
}
fn parse_role(s: &str) -> Result<MembershipRole, String> {
    s.parse().map_err(|e| format!("{e}"))
}
fn parse_packet_status(s: &str) -> Result<PacketStatus, String> {
    s.parse().map_err(|e| format!("{e}"))
}
fn parse_packet_kind(s: &str) -> Result<PacketKind, String> {
    s.parse().map_err(|e| format!("{e}"))
}
fn parse_token_kind(s: &str) -> Result<TokenKind, String> {
    TokenKind::parse(s).ok_or_else(|| format!("unknown token kind {s:?} (device, runner, admin)"))
}

/// `memvault admin <args...>` (users, spaces, memberships, tokens, consolidate, revert,
/// reindex, ...). `args` excludes `admin` itself. Prints JSON to stdout.
pub async fn run(state: AppState, args: Vec<String>) -> Result<()> {
    run_to(state, args, &mut std::io::stdout().lock()).await
}

/// [`run`] writing to `out`.
pub async fn run_to(state: AppState, args: Vec<String>, out: &mut dyn Write) -> Result<()> {
    let cli = Cli::try_parse_from(std::iter::once("memvault admin".to_string()).chain(args))?;
    execute(&state, cli.command, out).await
}

fn print<T: Serialize>(out: &mut dyn Write, value: &T) -> Result<()> {
    serde_json::to_writer_pretty(&mut *out, value)?;
    writeln!(out)?;
    Ok(())
}

async fn user_ref(conn: &mut sqlx::PgConnection, reference: &str) -> Result<Uuid> {
    let (id, handle) = match reference.parse::<Uuid>() {
        Ok(id) => (Some(id), None),
        Err(_) => (None, Some(reference)),
    };
    ops::resolve_user(conn, id, handle).await.map_err(|e| anyhow::anyhow!("{}", e.message))
}

fn api<T>(r: Result<T, memvault_core::ApiError>) -> Result<T> {
    r.map_err(|e| anyhow::anyhow!("{}: {}", e.code, e.message))
}

async fn execute(state: &AppState, command: Command, out: &mut dyn Write) -> Result<()> {
    let pool = &state.db.system;
    match command {
        Command::Consolidate(ConsolidateCommand::Now { space_id }) => {
            print(out, &api(run_planner(state, space_id).await)?)
        }
        command => {
            let mut tx = system_tx(pool).await.context("connecting to the database")?;
            let conn = &mut *tx;
            match command {
                Command::User(UserCommand::Create { handle, display_name }) => {
                    print(out, &api(ops::create_user(conn, &UserCreate { handle, display_name }).await)?)?
                }
                Command::User(UserCommand::List) => print(out, &api(ops::list_users(conn).await)?)?,
                Command::Space(SpaceCommand::Create { name, owner, kind }) => {
                    let owner_id = user_ref(conn, &owner).await?;
                    print(out, &api(ops::create_space(conn, &SpaceCreate { name, kind, owner_id }).await)?)?
                }
                Command::Space(SpaceCommand::List) => print(out, &api(ops::list_spaces(conn).await)?)?,
                Command::Membership(MembershipCommand::Add { space, user, role }) => {
                    let space_id = api(ops::resolve_space(conn, &space).await)?;
                    let user_id = user_ref(conn, &user).await?;
                    print(out, &api(ops::add_membership(conn, &MembershipCreate { space_id, user_id, role }).await)?)?
                }
                Command::Membership(MembershipCommand::List { space }) => {
                    let space_id = match space {
                        Some(s) => Some(api(ops::resolve_space(conn, &s).await)?),
                        None => None,
                    };
                    print(out, &api(ops::list_memberships(conn, space_id).await)?)?
                }
                Command::Token(TokenCommand::Create {
                    user,
                    client,
                    kind,
                    client_kind,
                    hostname,
                    platform,
                    expires_days,
                }) => {
                    let client_kind =
                        client_kind.map(|k| k.parse::<ClientKind>()).transpose().map_err(|e| anyhow::anyhow!("{e}"))?;
                    let body = TokenCreate {
                        user_id: user.parse().ok(),
                        handle: user.parse::<Uuid>().is_err().then_some(user),
                        device_id: client,
                        client_kind,
                        hostname,
                        platform,
                        kind,
                        expires_days,
                    };
                    let minted = api(ops::mint_token(conn, &state.settings.token_pepper, &body).await)?;
                    print(out, &minted)?;
                    eprintln!("Store the token now: it is not shown again.");
                }
                Command::Token(TokenCommand::Revoke { token }) => {
                    print(out, &api(ops::revoke_token(conn, &token).await)?)?
                }
                Command::Token(TokenCommand::List { user }) => {
                    let user_id = match user {
                        Some(u) => Some(user_ref(conn, &u).await?),
                        None => None,
                    };
                    print(out, &api(ops::list_tokens(conn, user_id).await)?)?
                }
                Command::Consolidate(ConsolidateCommand::Revert { run_id }) => {
                    print(out, &api(revert_run(conn, run_id, CLI_ACTOR).await)?)?
                }
                Command::Packets(PacketsCommand::List { status, kind, space_id, limit }) => {
                    print(out, &api(ops::list_packets(conn, status, kind, space_id, Some(limit)).await)?)?
                }
                Command::Packets(PacketsCommand::Retry { id, keep_attempts }) => {
                    print(out, &api(ops::retry_packet(conn, id, !keep_attempts).await)?)?
                }
                Command::Jobs(JobsCommand::List { status, job_type, limit }) => {
                    print(out, &api(ops::list_jobs(conn, status.as_deref(), job_type.as_deref(), Some(limit)).await)?)?
                }
                Command::Reindex { model, tables, space_id } => {
                    let req = ReindexRequest { model, tables: (!tables.is_empty()).then_some(tables), space_id };
                    print(out, &api(ops::reindex(conn, &req, &state.settings.embed_model).await)?)?
                }
                Command::Project(ProjectCommand::Remap { from, to, space_id }) => {
                    print(out, &api(ops::remap_project(conn, &RemapRequest { from, to, space_id }, CLI_ACTOR).await)?)?
                }
                Command::Consolidate(ConsolidateCommand::Now { .. }) => bail!("unreachable"),
            }
            tx.commit().await?;
            Ok(())
        }
    }
}
