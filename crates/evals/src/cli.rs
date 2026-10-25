//! The `memvault evals` command line.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use memvault_core::AppState;

use crate::replay::{EvalSet, Observer, ReplayOptions, Scratch, baseline_regression};
use crate::{compare, record, redact, replay, session};

#[derive(Debug, Parser)]
#[command(name = "memvault evals", about = "Evaluation harness (record, redact, replay, compare)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Export a session's events and packet outputs to evals/sessions/<id>.jsonl.
    Record {
        /// Session id, or the external (Claude Code) session id.
        session: String,
        #[arg(long, default_value = "evals/sessions")]
        out_dir: PathBuf,
    },
    /// Mask secrets, emails, tokens and home paths in recorded sessions (in place unless --out).
    Redact {
        files: Vec<PathBuf>,
        /// Write the result here instead of overwriting (one input file only).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Rebuild a scratch space from the recorded sessions and score recall.
    Replay {
        /// Directory with sessions/, questions.yaml, facts_gold.yaml and baseline.json.
        #[arg(long, default_value = "evals")]
        dir: PathBuf,
        /// Where packet outputs come from (default: recorded; claude when --prompt is given).
        #[arg(long, value_enum)]
        observer: Option<Observer>,
        /// Evaluate this observer prompt (`observer.code@v4`): the extract packets are pinned to it
        /// and run through `claude -p`.
        #[arg(long, value_name = "NAME@VERSION")]
        prompt: Option<String>,
        /// The `claude` executable for --prompt.
        #[arg(long, env = "CLAUDE_BIN")]
        claude_bin: Option<String>,
        /// Rerank the recall candidates (needs the reranker).
        #[arg(long)]
        rerank: bool,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
        /// Write the measured recall@5 to baseline.json instead of checking it.
        #[arg(long)]
        update_baseline: bool,
        /// Keep the scratch database.
        #[arg(long)]
        keep_scratch: bool,
    },
    /// Compare a claude-mem SQLite file with this service for the same session ids.
    Compare {
        #[arg(long)]
        claude_mem: PathBuf,
        /// External session ids; default: the recorded sessions in --dir.
        #[arg(long = "session")]
        sessions: Vec<String>,
        #[arg(long, default_value = "evals")]
        dir: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

/// `memvault evals <args...>` (`args` excludes `evals`).
pub async fn run(state: AppState, args: Vec<String>) -> Result<()> {
    let cli = Cli::try_parse_from(std::iter::once("evals".to_string()).chain(args))?;
    match cli.command {
        Command::Record { session, out_dir } => {
            let path = record::record(&state, &session, &out_dir).await?;
            println!("wrote {} (run `evals redact` before committing it)", path.display());
        }
        Command::Redact { files, out } => {
            if files.is_empty() {
                bail!("no files given");
            }
            if out.is_some() && files.len() > 1 {
                bail!("--out takes one input file");
            }
            for f in &files {
                let changed = redact::redact_file(f, out.as_deref())?;
                println!("{}: {changed} lines changed", f.display());
            }
        }
        Command::Replay { dir, observer, prompt, claude_bin, rerank, json, update_baseline, keep_scratch } => {
            let observer = observer.unwrap_or(if prompt.is_some() { Observer::Claude } else { Observer::Recorded });
            let set = EvalSet::load(&dir)?;
            let scratch = Scratch::create(&state).await?;
            let result =
                replay::replay(&scratch.state, &set, &ReplayOptions { observer, rerank, prompt, claude_bin }).await;
            if keep_scratch {
                eprintln!("kept scratch database {}", scratch.name());
            } else {
                scratch.drop_db().await?;
            }
            let report = result?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                report.print();
            }
            let baseline_path = dir.join("baseline.json");
            if update_baseline {
                let text = serde_json::to_string_pretty(&replay::Baseline { recall_at_5: report.recall_at_5 })?;
                std::fs::write(&baseline_path, text + "\n")?;
                println!("wrote {}", baseline_path.display());
            } else if let Some(msg) = set.baseline.as_ref().and_then(|b| baseline_regression(&report, b)) {
                bail!("{msg}");
            }
        }
        Command::Compare { claude_mem, mut sessions, dir, json } => {
            if sessions.is_empty() {
                sessions =
                    session::load_dir(&dir.join("sessions"))?.into_iter().map(|s| s.external_session_id).collect();
            }
            let rows = compare::compare(&state, &claude_mem, &sessions).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                compare::print(&rows);
            }
        }
    }
    Ok(())
}
