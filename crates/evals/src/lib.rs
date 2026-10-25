//! memvault-evals: record, redact, replay and compare (spec 16.2, 16.3), run as
//! `memvault evals <subcommand>`.
//!
//! - [`record`]: export a session's events and packet outputs to `evals/sessions/<id>.jsonl`.
//! - [`redact`]: mask secrets, emails, tokens and home paths in recorded sessions.
//! - [`replay`]: rebuild a scratch space from the sessions (with the recorded or fake observer, or
//!   `--prompt name@version` run through `claude -p`), run `evals/questions.yaml` through recall and
//!   score it against `evals/facts_gold.yaml` and `evals/baseline.json`.
//! - [`compare`]: claude-mem's SQLite and this service side by side for the same session ids.

pub mod claude;
pub mod cli;
pub mod compare;
pub mod record;
pub mod redact;
pub mod replay;
pub mod session;
