//! memvault-consolidate: planner, consolidation appliers, revert, admin routes and CLI (spec 6.7,
//! 12).
//!
//! - [`planner`]: the nightly pass (`memvault planner`), operator runs (`POST /v1/admin/consolidate`) and the
//!   scoped runs the worker makes after a session ends or a scope fills up: fact clusters, auto-activation,
//!   decay and archival, day/week summaries, procedure mining, profiles.
//! - [`appliers`]: results of `consolidate_facts`, `consolidate_summary`, `consolidate_procedures`
//!   packets, applied by code with audit rows.
//! - [`revert`]: undo a run from its audit rows.
//! - [`admin`], [`admin_cli`], [`ops`]: users, spaces, memberships, tokens, packets, jobs, reindex,
//!   project remap, over REST and the command line.
//! - [`profiles`]: cached profile blocks.

use axum::Router;
use memvault_core::{AppState, AppliersRegistry};

pub mod admin;
pub mod admin_cli;
pub mod appliers;
pub mod audit;
pub mod doctor;
pub mod ops;
pub mod planner;
pub mod profiles;
pub mod revert;

/// Routes under `/v1` (paths without the prefix): `/admin/...`.
pub fn router() -> Router<AppState> {
    admin::router()
}

/// Register the `consolidate_facts`, `consolidate_summary` and `consolidate_procedures` appliers.
pub fn register_appliers(registry: &mut AppliersRegistry) {
    appliers::register(registry);
}
