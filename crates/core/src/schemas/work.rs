//! Spec 6.6 / 9: work packets as seen by runners (lease, result, fail, heartbeat) and by the server.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::appliers::AppliedCounts;
use crate::error::ApiError;
use crate::schemas::common::{OutputSchemaName, PacketKind, PacketStatus};

pub const DEFAULT_LEASE_KINDS: [PacketKind; 3] =
    [PacketKind::Extract, PacketKind::SummarizePrompt, PacketKind::SummarizeSession];

fn default_kinds() -> Vec<PacketKind> {
    DEFAULT_LEASE_KINDS.to_vec()
}

fn one() -> i64 {
    1
}

fn default_lease_seconds() -> i64 {
    600
}

/// `POST /v1/work/lease` (work:lease).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseRequest {
    /// Restrict to one session's packets; `None` = any packet the caller may run.
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default = "default_kinds")]
    pub kinds: Vec<PacketKind>,
    #[serde(default = "one")]
    pub max: i64,
    #[serde(default = "default_lease_seconds")]
    pub lease_seconds: i64,
    /// How the runner calls the model (`claude-p`, `api`, ...); only labels the leased metric.
    #[serde(default)]
    pub runner_kind: Option<String>,
}

impl Default for LeaseRequest {
    fn default() -> Self {
        Self {
            session_id: None,
            kinds: default_kinds(),
            max: 1,
            lease_seconds: default_lease_seconds(),
            runner_kind: None,
        }
    }
}

impl LeaseRequest {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.kinds.is_empty() {
            return Err(ApiError::invalid("kinds must not be empty"));
        }
        if !(1..=50).contains(&self.max) {
            return Err(ApiError::invalid("max must be 1-50"));
        }
        if !(30..=3600).contains(&self.lease_seconds) {
            return Err(ApiError::invalid("lease_seconds must be 30-3600"));
        }
        Ok(())
    }
}

/// Rendered prompt so runners need no prompt files. `user` is `user_template` rendered with the
/// packet's inputs (what `mem-hook observe` passes to `claude -p`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacketPrompt {
    pub system: String,
    pub user_template: String,
    pub user: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeasedPacket {
    pub id: Uuid,
    pub kind: PacketKind,
    pub prompt_ref: String,
    pub model_hint: String,
    pub inputs: serde_json::Value,
    pub output_schema: OutputSchemaName,
    pub lease_until: DateTime<Utc>,
    #[serde(default)]
    pub attempts: i32,
    pub prompt: PacketPrompt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseResponse {
    pub packets: Vec<LeasedPacket>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResultMeta {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// `claude-p`.
    #[serde(default)]
    pub runner_kind: Option<String>,
    /// Device id.
    #[serde(default)]
    pub runner: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `POST /v1/work/{id}/result` (work:lease). `output` is the model's JSON object, or its raw text
/// (one surrounding code fence is stripped, spec 9.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultRequest {
    pub output: serde_json::Value,
    #[serde(default)]
    pub meta: ResultMeta,
}

/// 200. A duplicate post returns the first application's counts with `duplicate = true` (9.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultResponse {
    pub applied: AppliedCounts,
    #[serde(default)]
    pub duplicate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    QuotaExhausted,
    CliMissing,
    Timeout,
    Other,
}

/// `POST /v1/work/{id}/fail` (work:lease).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailRequest {
    pub error: FailReason,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailResponse {
    pub id: Uuid,
    pub status: PacketStatus,
    pub attempts: i32,
    #[serde(default)]
    pub available_after: Option<DateTime<Utc>>,
}

/// `POST /v1/work/{id}/heartbeat` (work:lease).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub id: Uuid,
    pub lease_until: DateTime<Utc>,
}

/// A `work_packets` row (`sqlx::query_as::<_, PacketRecord>("SELECT * FROM work_packets ...")`
/// selects the foundation columns by name). Passed to appliers and returned by
/// `GET /v1/admin/packets`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct PacketRecord {
    pub id: Uuid,
    pub space_id: Uuid,
    pub kind: PacketKind,
    pub session_id: Option<Uuid>,
    pub preferred_client_id: Option<Uuid>,
    pub prompt_ref: String,
    pub model_hint: String,
    pub inputs: serde_json::Value,
    pub output_schema: OutputSchemaName,
    pub status: PacketStatus,
    pub attempts: i32,
    pub max_attempts: i32,
    pub leased_by: Option<Uuid>,
    pub lease_until: Option<DateTime<Utc>>,
    pub available_after: DateTime<Utc>,
    pub result: Option<serde_json::Value>,
    pub result_meta: Option<serde_json::Value>,
    pub last_error: Option<String>,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
    pub done_at: Option<DateTime<Utc>>,
}
