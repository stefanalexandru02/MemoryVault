//! MemoryVault foundation shared by every crate: settings, database pools and RLS transactions,
//! ids, bearer-token auth, request/response and LLM-output schemas, the TEI client (and its fake),
//! the prompt registry, the packet-applier registry, cache invalidation, errors and [`AppState`].
//!
//! See `CONTRACTS.md` at the repository root for the conventions module crates code against.

pub mod appliers;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod http;
pub mod ids;
pub mod invalidate;
pub mod jobs;
pub mod metrics;
pub mod prompts;
pub mod schemas;
pub mod state;
pub mod tei;
#[cfg(feature = "test-support")]
pub mod test_support;

pub use appliers::{AppliedCounts, AppliersRegistry, PacketApplier};
pub use auth::{Identity, require_scope};
pub use config::Settings;
pub use db::{Db, system_tx, user_tx};
pub use error::ApiError;
pub use schemas::common::estimate_tokens;
pub use state::AppState;
