//! Appliers of `consolidate_*` results (spec 12.2, 12.3). Code applies the model's decisions; the
//! model never writes SQL. Every change is audited with the packet's run id and stays inside the
//! packet's space.

mod facts;
mod procedures;
mod summary;

use memvault_core::schemas::common::PacketKind;
use memvault_core::schemas::outputs::LlmOutput;
use memvault_core::schemas::work::PacketRecord;
use memvault_core::{ApiError, AppliersRegistry};
use uuid::Uuid;

use crate::audit::AuditCtx;

pub use facts::FactsApplier;
pub use procedures::ProceduresApplier;
pub use summary::SummaryApplier;

/// Register the `consolidate_facts`, `consolidate_summary` and `consolidate_procedures` appliers.
pub fn register(registry: &mut AppliersRegistry) {
    registry.register(PacketKind::ConsolidateFacts, FactsApplier);
    registry.register(PacketKind::ConsolidateSummary, SummaryApplier);
    registry.register(PacketKind::ConsolidateProcedures, ProceduresApplier);
}

/// The planner run a packet belongs to (`inputs.run_id`); a packet without one is its own run.
pub fn run_id_of(packet: &PacketRecord) -> Uuid {
    packet.inputs.get("run_id").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(packet.id)
}

/// Audit context for changes made by applying `packet`.
pub fn packet_ctx(packet: &PacketRecord) -> AuditCtx {
    AuditCtx {
        space_id: packet.space_id,
        actor: format!("packet:{}", packet.id),
        run_id: run_id_of(packet),
        model: packet.result_meta.as_ref().and_then(|m| m.get("model")).and_then(|m| m.as_str()).map(String::from),
        prompt_version: Some(packet.prompt_ref.clone()),
    }
}

/// `extracted_by` of rows derived from a consolidation packet (same shape as spec 4.3).
pub fn extracted_by(packet: &PacketRecord) -> serde_json::Value {
    let meta = |key: &str| packet.result_meta.as_ref().and_then(|m| m.get(key)).and_then(|v| v.as_str());
    serde_json::json!({
        "runner": meta("runner"),
        "runner_kind": meta("runner_kind"),
        "model": meta("model"),
        "prompt": packet.prompt_ref,
        "packet_id": packet.id,
    })
}

fn wrong_output(packet: &PacketRecord, output: &LlmOutput) -> ApiError {
    ApiError::internal(format!("packet {} ({}) got a {} output", packet.id, packet.kind, output.schema()))
}
