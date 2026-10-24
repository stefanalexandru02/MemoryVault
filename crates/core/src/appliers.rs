//! Result appliers keyed by packet kind (spec 9.4, 10.4, 10.5, 12.2, 12.3).
//!
//! memvault-ingest registers `extract`, `summarize_prompt`, `summarize_session`;
//! memvault-consolidate registers the `consolidate_*` kinds (each crate's
//! `register_appliers(&mut AppliersRegistry)`, called by the server when it builds the
//! [`crate::AppState`]). The work-result route validates the output against the packet's
//! `output_schema` and then calls [`AppliersRegistry::apply`] in the same transaction that marks
//! the packet done.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgConnection;

use crate::error::ApiError;
use crate::schemas::common::PacketKind;
use crate::schemas::outputs::LlmOutput;
use crate::schemas::work::PacketRecord;

/// Counts for the result response, e.g. `{"observations": 2, "fact_candidates": 1}`.
pub type AppliedCounts = BTreeMap<String, i64>;

#[async_trait]
pub trait PacketApplier: Send + Sync {
    /// Apply a validated output inside `conn`'s transaction (a `system_tx`).
    async fn apply(
        &self,
        conn: &mut PgConnection,
        packet: &PacketRecord,
        output: &LlmOutput,
    ) -> Result<AppliedCounts, ApiError>;
}

#[derive(Default, Clone)]
pub struct AppliersRegistry {
    appliers: HashMap<PacketKind, Arc<dyn PacketApplier>>,
}

impl std::fmt::Debug for AppliersRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppliersRegistry").field("kinds", &self.kinds()).finish()
    }
}

impl AppliersRegistry {
    /// Register `applier` for `kind` (replaces an earlier registration).
    pub fn register(&mut self, kind: PacketKind, applier: impl PacketApplier + 'static) {
        self.appliers.insert(kind, Arc::new(applier));
    }

    pub fn get(&self, kind: PacketKind) -> Option<&Arc<dyn PacketApplier>> {
        self.appliers.get(&kind)
    }

    pub fn kinds(&self) -> Vec<PacketKind> {
        let mut kinds: Vec<_> = self.appliers.keys().copied().collect();
        kinds.sort();
        kinds
    }

    /// Apply a validated output with the applier registered for `packet.kind` (500 if none).
    pub async fn apply(
        &self,
        conn: &mut PgConnection,
        packet: &PacketRecord,
        output: &LlmOutput,
    ) -> Result<AppliedCounts, ApiError> {
        let applier = self
            .get(packet.kind)
            .ok_or_else(|| ApiError::internal(format!("no applier registered for packet kind {}", packet.kind)))?;
        applier.apply(conn, packet, output).await
    }
}
