//! Spec 6.3: push-mode recall, context block, feedback. Slot item shapes are the recall module's contract.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::error::ApiError;
use crate::schemas::common::{InjectionMode, Scope, Sensitivity, SummaryGrain};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskState {
    /// Matched against `procedures.task_class` (+0.3, spec 11.2).
    #[serde(default)]
    pub intent: Option<String>,
    /// Appended to the keyword query with lower weight.
    #[serde(default)]
    pub entities: Vec<String>,
}

/// Token budget per slot (spec 11.5). 0 disables a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Budgets {
    pub profile: u32,
    pub facts: u32,
    pub procedures: u32,
    pub observations: u32,
    pub summaries: u32,
    pub work_state: u32,
    /// Ingested documents; never part of the SessionStart block.
    pub documents: u32,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            profile: 400,
            facts: 300,
            procedures: 400,
            observations: 600,
            summaries: 300,
            work_state: 200,
            documents: 400,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecallFormat {
    Json,
    Markdown,
    #[default]
    Both,
}

fn default_expand() -> u32 {
    3
}

fn default_true() -> bool {
    true
}

fn default_exclude() -> Vec<Sensitivity> {
    vec![Sensitivity::Secret]
}

/// `POST /v1/recall` (memories:read).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallRequest {
    /// Default: readable spaces (default + shared), spec 5.3.
    #[serde(default)]
    pub space_ids: Option<Vec<Uuid>>,
    /// Default: global + the caller's device; recall decides.
    #[serde(default)]
    pub scopes: Option<Vec<Scope>>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub task_state: Option<TaskState>,
    #[serde(default)]
    pub budgets: Budgets,
    #[serde(default = "default_expand")]
    pub expand_top_observations: u32,
    #[serde(default = "default_true")]
    pub rerank: bool,
    #[serde(default = "default_exclude")]
    pub exclude_sensitivity: Vec<Sensitivity>,
    #[serde(default)]
    pub format: RecallFormat,
    /// Point in time (RFC 3339): recall what was valid then instead of now.
    #[serde(default)]
    pub as_of: Option<DateTime<Utc>>,
}

impl Default for RecallRequest {
    fn default() -> Self {
        Self {
            space_ids: None,
            scopes: None,
            query: None,
            task_state: None,
            budgets: Budgets::default(),
            expand_top_observations: default_expand(),
            rerank: true,
            exclude_sensitivity: default_exclude(),
            format: RecallFormat::Both,
            as_of: None,
        }
    }
}

impl RecallRequest {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.expand_top_observations > 20 {
            return Err(ApiError::invalid("expand_top_observations must be <= 20"));
        }
        Ok(())
    }
}

/// Common fields of every ranked slot item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotItem {
    pub id: Uuid,
    pub space_id: Uuid,
    pub scope: Scope,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub token_estimate: i32,
    /// e.g. `[fact#a91c2e]`, or `[fact#a91c2e · unconfirmed 4 months]` for a fact to treat with care.
    #[serde(default)]
    pub citation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileSlot {
    pub body_md: String,
    pub token_estimate: i32,
    #[serde(default)]
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactItem {
    #[serde(flatten)]
    pub item: SlotItem,
    /// Entity `canonical_name`.
    pub subject: String,
    #[serde(default)]
    pub subject_kind: Option<String>,
    pub predicate: String,
    #[serde(default)]
    pub value: serde_json::Value,
    pub value_text: String,
    pub confidence: f64,
    #[serde(default)]
    pub importance: f64,
    #[serde(default)]
    pub pinned: bool,
    /// Device ids.
    #[serde(default)]
    pub learned_on: Vec<String>,
    #[serde(default)]
    pub first_seen_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_confirmed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureItem {
    #[serde(flatten)]
    pub item: SlotItem,
    pub name: String,
    pub task_class: String,
    pub trigger_text: String,
    pub body_md: String,
    #[serde(default)]
    pub version: i32,
    #[serde(default)]
    pub success_count: i32,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationItem {
    #[serde(flatten)]
    pub item: SlotItem,
    #[serde(rename = "type")]
    pub obs_type: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Present only when expanded.
    #[serde(default)]
    pub narrative: Option<String>,
    /// Present only when expanded.
    #[serde(default)]
    pub facts: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub importance: f64,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryItem {
    #[serde(flatten)]
    pub item: SlotItem,
    pub grain: SummaryGrain,
    #[serde(default)]
    pub request: Option<String>,
    #[serde(default)]
    pub learned: Option<String>,
    #[serde(default)]
    pub completed: Option<String>,
    #[serde(default)]
    pub next_steps: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub device_id: Option<String>,
}

/// A chunk of an ingested document. The text is the whole chunk; the rendered markdown shows an excerpt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentItem {
    #[serde(flatten)]
    pub item: SlotItem,
    /// Where it comes from, `uri#Heading > Subheading` (the bare uri above the first heading).
    pub source: String,
    pub uri: String,
    pub title: String,
    #[serde(default)]
    pub heading_path: Vec<String>,
    pub chunk_index: i32,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

/// A folded work-state list (last write per key wins), spec 6.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkStateList {
    pub scope: Scope,
    pub list_name: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub token_estimate: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecallSlots {
    #[serde(default)]
    pub profile: Option<ProfileSlot>,
    #[serde(default)]
    pub facts: Vec<FactItem>,
    #[serde(default)]
    pub procedures: Vec<ProcedureItem>,
    #[serde(default)]
    pub observations: Vec<ObservationItem>,
    #[serde(default)]
    pub summaries: Vec<SummaryItem>,
    #[serde(default)]
    pub work_state: Vec<WorkStateList>,
    #[serde(default)]
    pub documents: Vec<DocumentItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub total: i32,
    #[serde(default)]
    pub by_slot: BTreeMap<String, i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallResponse {
    pub retrieval_id: Uuid,
    pub slots: RecallSlots,
    /// `None` when `format == "json"`.
    #[serde(default)]
    pub rendered_markdown: Option<String>,
    pub tokens: TokenUsage,
    pub latency_ms: i64,
    pub reranked: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextSource {
    #[default]
    Startup,
    Resume,
    Clear,
    Compact,
}

impl ContextSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextSource::Startup => "startup",
            ContextSource::Resume => "resume",
            ContextSource::Clear => "clear",
            ContextSource::Compact => "compact",
        }
    }
}

/// `GET /v1/context` query params; the response is `text/markdown` with an ETag.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextQuery {
    #[serde(default)]
    pub project_key: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub source: ContextSource,
    /// Overrides the user's default injection mode for this request.
    #[serde(default)]
    pub mode: Option<InjectionMode>,
}

/// `GET`/`PUT /v1/me/settings`: the caller's server-side defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSettings {
    /// Injection mode of `GET /v1/context` when neither the request nor the client config names one.
    pub injection_mode: InjectionMode,
}

/// `GET /v1/stats/recall` query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsQuery {
    /// Window in days (default 7, at most 90).
    #[serde(default)]
    pub days: Option<u32>,
    /// Admin tokens only: every user's calls instead of the caller's own.
    #[serde(default)]
    pub all: bool,
}

/// Tokens served by one kind of call (`context` or `recall`) in one injection mode over the window.
/// `injection` is empty for `recall`, which has no mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ModeStats {
    pub mode: String,
    pub injection: Option<InjectionMode>,
    pub calls: i64,
    pub avg_tokens: f64,
    pub p50_tokens: f64,
    pub p95_tokens: f64,
    pub avg_hits: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallStats {
    pub days: u32,
    /// True when the numbers cover every user (admin with `all=true`).
    pub all_users: bool,
    pub modes: Vec<ModeStats>,
}

/// `POST /v1/recall/{retrieval_id}/feedback` (memories:write).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedbackRequest {
    #[serde(default)]
    pub used_ids: Vec<Uuid>,
    #[serde(default)]
    pub helpful: Option<bool>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackResponse {
    pub retrieval_id: Uuid,
    pub recorded: bool,
}
