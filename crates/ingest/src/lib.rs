//! memvault-ingest: sessions, events, windows, work packets and the extract/summary appliers
//! (spec 6.1, 6.2, 6.6, 9, 10.4, 10.5).
//!
//! - [`sessions`]: `POST /v1/sessions`, `POST /v1/sessions/{id}/end`.
//! - [`events`]: `POST /v1/sessions/{id}/events` (clamping, idempotency, window assignment).
//! - [`windows`]: window state per session (`session_windows`, migration 0100) and the packets a
//!   closing window owes; [`windows::close_idle_windows`] is the worker sweep.
//! - [`queue`]: packet creation and the lease protocol; [`work`]: its routes.
//! - [`extract`], [`summaries`]: result appliers, registered by [`register_appliers`].
//! - [`prompts`]: `GET /v1/prompts/{name}`.

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use memvault_core::schemas::common::PacketKind;
use memvault_core::schemas::events::MAX_BODY_BYTES;
use memvault_core::{AppState, AppliersRegistry};

pub mod builders;
pub mod events;
pub mod extract;
pub mod prompts;
pub mod queue;
pub mod sessions;
pub mod summaries;
mod tx;
pub mod windows;
pub mod work;

/// Routes under `/v1` (paths without the prefix): sessions, events, work, prompts.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sessions", post(sessions::create_session))
        .route("/sessions/{id}/end", post(sessions::end_session))
        .route("/sessions/{id}/events", post(events::post_events).layer(DefaultBodyLimit::max(MAX_BODY_BYTES)))
        .route("/work/lease", post(work::lease))
        .route("/work/{id}/result", post(work::post_result))
        .route("/work/{id}/fail", post(work::post_fail))
        .route("/work/{id}/heartbeat", post(work::post_heartbeat))
        .route("/prompts/{name}", get(prompts::get_prompt))
}

/// Register the `extract`, `summarize_prompt` and `summarize_session` appliers.
pub fn register_appliers(registry: &mut AppliersRegistry) {
    registry.register(PacketKind::Extract, extract::ExtractApplier);
    registry.register(PacketKind::SummarizePrompt, summaries::SummaryApplier::prompt());
    registry.register(PacketKind::SummarizeSession, summaries::SummaryApplier::session());
}
