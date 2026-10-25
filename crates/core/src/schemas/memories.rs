//! Spec 6.4 (memories), 12.5 (explain).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::schemas::common::{
    Concept, FactStatus, MemoryTable, ObservationType, Scope, Sensitivity, SummaryGrain, comma_list,
};

pub const MAX_GET_IDS: usize = 20;
pub const PREDICATE_PATTERN: &str = r"^[a-z0-9_]+(\.[a-z0-9_]+)*$";

/// `^[a-z0-9_]+(\.[a-z0-9_]+)*$` (dotted, lowercase).
pub fn is_valid_predicate(p: &str) -> bool {
    !p.is_empty()
        && p.split('.').all(|seg| {
            !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

// ---------- search / timeline / by-file: index rows ----------

fn default_limit() -> i64 {
    20
}

/// `GET /v1/search` query params (list params comma-separated in the URL). Requires `q` or at
/// least one filter (types, since, until, files, scopes, as_of).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default, deserialize_with = "comma_list::deserialize")]
    pub space_ids: Option<Vec<Uuid>>,
    #[serde(default, deserialize_with = "comma_list::deserialize")]
    pub scopes: Option<Vec<Scope>>,
    #[serde(default, deserialize_with = "comma_list::deserialize")]
    pub types: Option<Vec<String>>,
    /// Default: observations and summaries.
    #[serde(default, deserialize_with = "comma_list::deserialize")]
    pub tables: Option<Vec<MemoryTable>>,
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "comma_list::deserialize")]
    pub files: Option<Vec<String>>,
    /// Point in time (RFC 3339): only what was valid then. Facts and procedures count when their
    /// validity covers the instant, observations and summaries when they already existed.
    #[serde(default)]
    pub as_of: Option<DateTime<Utc>>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// Largest `offset + limit` a search accepts.
pub const MAX_SEARCH_WINDOW: i64 = 1000;

impl SearchQuery {
    pub fn validate(&self) -> Result<(), ApiError> {
        let has_q = self.q.as_deref().is_some_and(|q| !q.trim().is_empty());
        let has_filter = self.types.is_some()
            || self.since.is_some()
            || self.until.is_some()
            || self.files.is_some()
            || self.scopes.is_some()
            || self.as_of.is_some();
        if !has_q && !has_filter {
            return Err(ApiError::invalid("search requires q or at least one filter"));
        }
        if !(1..=100).contains(&self.limit) {
            return Err(ApiError::invalid("limit must be 1-100"));
        }
        if self.offset < 0 {
            return Err(ApiError::invalid("offset must be >= 0"));
        }
        // Every leg fetches offset + limit candidates, so the window is bounded.
        if self.offset > MAX_SEARCH_WINDOW - self.limit {
            return Err(ApiError::invalid(format!("offset + limit must be <= {MAX_SEARCH_WINDOW}")));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexRow {
    pub id: Uuid,
    pub table: MemoryTable,
    /// Observation type, summary grain, fact predicate, procedure task_class.
    #[serde(rename = "type", default)]
    pub row_type: Option<String>,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub scope: Scope,
    #[serde(default)]
    pub token_estimate: i32,
    #[serde(default)]
    pub score: Option<f64>,
    /// Timeline only.
    #[serde(default)]
    pub is_anchor: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub rows: Vec<IndexRow>,
    pub limit: i64,
    pub offset: i64,
}

fn default_five() -> i64 {
    5
}

/// `GET /v1/timeline?anchor_id=…&before=5&after=5`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineQuery {
    pub anchor_id: Uuid,
    #[serde(default = "default_five")]
    pub before: i64,
    #[serde(default = "default_five")]
    pub after: i64,
}

impl TimelineQuery {
    pub fn validate(&self) -> Result<(), ApiError> {
        if !(0..=50).contains(&self.before) || !(0..=50).contains(&self.after) {
            return Err(ApiError::invalid("before and after must be 0-50"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineResponse {
    pub anchor_id: Uuid,
    /// Time order; the anchor row has `is_anchor = true`.
    pub rows: Vec<IndexRow>,
}

fn default_by_file_limit() -> i64 {
    15
}

/// `GET /v1/observations/by-file?path=…&project_key=…&limit=15`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ByFileQuery {
    pub path: String,
    #[serde(default)]
    pub project_key: Option<String>,
    #[serde(default = "default_by_file_limit")]
    pub limit: i64,
}

impl ByFileQuery {
    pub fn validate(&self) -> Result<(), ApiError> {
        if !(1..=50).contains(&self.limit) {
            return Err(ApiError::invalid("limit must be 1-50"));
        }
        Ok(())
    }
}

// ---------- full bodies ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityRef {
    pub id: Uuid,
    pub kind: String,
    pub canonical_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationFull {
    pub id: Uuid,
    pub space_id: Uuid,
    pub scope: Scope,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub window_no: Option<i32>,
    #[serde(rename = "type")]
    pub obs_type: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub narrative: Option<String>,
    #[serde(default)]
    pub facts: Vec<String>,
    #[serde(default)]
    pub concepts: Vec<String>,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    pub importance: f64,
    #[serde(default = "normal")]
    pub sensitivity: Sensitivity,
    #[serde(default)]
    pub source_event_ids: Vec<Uuid>,
    #[serde(default)]
    pub extracted_by: serde_json::Value,
    #[serde(default)]
    pub written_by_client_id: Option<Uuid>,
    #[serde(default)]
    pub token_estimate: i32,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub archived_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub superseded_by: Option<Uuid>,
}

fn normal() -> Sensitivity {
    Sensitivity::Normal
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryFull {
    pub id: Uuid,
    pub space_id: Uuid,
    pub scope: Scope,
    pub grain: SummaryGrain,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub prompt_no: Option<i32>,
    #[serde(default)]
    pub period_start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub period_end: Option<DateTime<Utc>>,
    #[serde(default)]
    pub request: Option<String>,
    #[serde(default)]
    pub investigated: Option<String>,
    #[serde(default)]
    pub learned: Option<String>,
    #[serde(default)]
    pub completed: Option<String>,
    #[serde(default)]
    pub next_steps: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub extracted_by: serde_json::Value,
    #[serde(default)]
    pub token_estimate: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactFull {
    pub id: Uuid,
    pub space_id: Uuid,
    pub scope: Scope,
    pub subject: EntityRef,
    pub predicate: String,
    #[serde(default)]
    pub value: serde_json::Value,
    pub value_text: String,
    pub confidence: f64,
    pub importance: f64,
    pub status: FactStatus,
    #[serde(default)]
    pub conflict: bool,
    #[serde(default)]
    pub supersedes_id: Option<Uuid>,
    #[serde(default)]
    pub superseded_by: Option<Uuid>,
    #[serde(default)]
    pub valid_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub valid_to: Option<DateTime<Utc>>,
    pub first_seen_at: DateTime<Utc>,
    pub last_confirmed_at: DateTime<Utc>,
    #[serde(default)]
    pub confirmations: i32,
    #[serde(default)]
    pub evidence_observation_ids: Vec<Uuid>,
    #[serde(default)]
    pub learned_on_client_ids: Vec<Uuid>,
    #[serde(default)]
    pub access_count: i32,
    #[serde(default)]
    pub last_accessed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default = "normal")]
    pub sensitivity: Sensitivity,
    #[serde(default)]
    pub written_by_client_id: Option<Uuid>,
    #[serde(default)]
    pub extracted_by: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcedureStatus {
    #[default]
    Active,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureFull {
    pub id: Uuid,
    pub space_id: Uuid,
    pub scope: Scope,
    pub name: String,
    pub task_class: String,
    pub trigger_text: String,
    pub body_md: String,
    #[serde(default)]
    pub version: i32,
    #[serde(default)]
    pub status: ProcedureStatus,
    #[serde(default)]
    pub supersedes_id: Option<Uuid>,
    #[serde(default)]
    pub success_count: i32,
    #[serde(default)]
    pub failure_count: i32,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub source_observation_ids: Vec<Uuid>,
    #[serde(default)]
    pub token_estimate: i32,
    pub created_at: DateTime<Utc>,
}

/// A full memory body, tagged by `table`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "table", rename_all = "lowercase")]
pub enum MemoryItem {
    Observations(ObservationFull),
    Summaries(SummaryFull),
    Facts(FactFull),
    Procedures(ProcedureFull),
}

impl MemoryItem {
    pub fn id(&self) -> Uuid {
        match self {
            MemoryItem::Observations(o) => o.id,
            MemoryItem::Summaries(s) => s.id,
            MemoryItem::Facts(f) => f.id,
            MemoryItem::Procedures(p) => p.id,
        }
    }

    pub fn table(&self) -> MemoryTable {
        match self {
            MemoryItem::Observations(_) => MemoryTable::Observations,
            MemoryItem::Summaries(_) => MemoryTable::Summaries,
            MemoryItem::Facts(_) => MemoryTable::Facts,
            MemoryItem::Procedures(_) => MemoryTable::Procedures,
        }
    }
}

/// `POST /v1/memories/get` (≤ 20 ids).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetRequest {
    pub ids: Vec<Uuid>,
}

impl GetRequest {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.ids.is_empty() || self.ids.len() > MAX_GET_IDS {
            return Err(ApiError::invalid(format!("ids must hold 1-{MAX_GET_IDS} items")));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetResponse {
    pub items: Vec<MemoryItem>,
    /// Unknown or not visible to the caller.
    #[serde(default)]
    pub missing: Vec<Uuid>,
}

// ---------- explain (12.5) ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: Uuid,
    pub harness: String,
    #[serde(default)]
    pub project_key: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub client_kind: Option<String>,
    pub started_at: DateTime<Utc>,
}

/// Where an observation/summary came from: its session, raw event seqs, and the extractor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    #[serde(default)]
    pub session: Option<SessionInfo>,
    #[serde(default)]
    pub event_ids: Vec<Uuid>,
    #[serde(default)]
    pub event_seqs: Vec<i32>,
    #[serde(default)]
    pub extracted_by: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceObservation {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub obs_type: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: Uuid,
    pub actor: String,
    pub action: String,
    #[serde(default)]
    pub before: Option<serde_json::Value>,
    #[serde(default)]
    pub after: Option<serde_json::Value>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub prompt_version: Option<String>,
    #[serde(default)]
    pub run_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Facts/procedures: `evidence` lists the observations they rest on, each with provenance.
/// Observations/summaries: `provenance` is filled directly and `evidence` is empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplainResponse {
    pub id: Uuid,
    pub table: MemoryTable,
    pub item: MemoryItem,
    #[serde(default)]
    pub provenance: Option<Provenance>,
    #[serde(default)]
    pub evidence: Vec<EvidenceObservation>,
    #[serde(default)]
    pub audit: Vec<AuditEntry>,
}

// ---------- history ----------

/// One link of a supersession chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: Uuid,
    pub table: MemoryTable,
    /// Fact `predicate: value`, procedure name and version with the start of its body.
    pub text: String,
    /// Fact status, procedure status, `archived` or `active` for observations and summaries.
    pub status: String,
    #[serde(default)]
    pub valid_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub valid_to: Option<DateTime<Utc>>,
    #[serde(default)]
    pub supersedes_id: Option<Uuid>,
    #[serde(default)]
    pub superseded_by: Option<Uuid>,
    /// The id the history was asked for.
    pub current: bool,
    /// Audit trail of the entry's supersession (or retraction): when, by whom, why.
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ended_by: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub run_id: Option<Uuid>,
}

/// `GET /v1/memories/{id}/history`: the chain oldest to newest, the asked-for id included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub id: Uuid,
    pub table: MemoryTable,
    pub entries: Vec<HistoryEntry>,
}

// ---------- explicit writes ----------

fn default_subject_kind() -> String {
    "user".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactSubject {
    #[serde(default = "default_subject_kind")]
    pub kind: String,
    pub name: String,
}

/// Lands `active` with confidence 0.95 and `written_by_client_id` set (spec 6.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RememberFact {
    /// Default: project scope of the caller's current project, else global.
    #[serde(default)]
    pub scope: Option<Scope>,
    /// Default: the caller's default space.
    #[serde(default)]
    pub space_id: Option<Uuid>,
    #[serde(default = "normal")]
    pub sensitivity: Sensitivity,
    pub subject: FactSubject,
    pub predicate: String,
    #[serde(default)]
    pub value: serde_json::Value,
    pub value_text: String,
    #[serde(default)]
    pub pinned: bool,
}

fn default_obs_type() -> ObservationType {
    ObservationType::Discovery
}

fn default_remember_importance() -> f64 {
    0.7
}

/// `extracted_by = {"runner_kind": "explicit"}`. MCP `memory_remember(kind="note")` maps here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RememberObservation {
    #[serde(default)]
    pub scope: Option<Scope>,
    #[serde(default)]
    pub space_id: Option<Uuid>,
    #[serde(default = "normal")]
    pub sensitivity: Sensitivity,
    #[serde(rename = "type", default = "default_obs_type")]
    pub obs_type: ObservationType,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub narrative: Option<String>,
    #[serde(default)]
    pub facts: Vec<String>,
    #[serde(default)]
    pub concepts: Vec<Concept>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default = "default_remember_importance")]
    pub importance: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RememberProcedure {
    #[serde(default)]
    pub scope: Option<Scope>,
    #[serde(default)]
    pub space_id: Option<Uuid>,
    #[serde(default = "normal")]
    pub sensitivity: Sensitivity,
    pub name: String,
    pub task_class: String,
    pub trigger_text: String,
    pub body_md: String,
}

/// `POST /v1/memories` (memories:write), tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RememberRequest {
    Fact(RememberFact),
    Observation(RememberObservation),
    Procedure(RememberProcedure),
}

fn char_range(name: &str, s: &str, min: usize, max: usize) -> Result<(), ApiError> {
    let n = s.chars().count();
    if n < min || n > max {
        return Err(ApiError::invalid(format!("{name} must be {min}-{max} characters")));
    }
    Ok(())
}

impl RememberRequest {
    pub fn scope(&self) -> Option<&Scope> {
        match self {
            RememberRequest::Fact(f) => f.scope.as_ref(),
            RememberRequest::Observation(o) => o.scope.as_ref(),
            RememberRequest::Procedure(p) => p.scope.as_ref(),
        }
    }

    pub fn space_id(&self) -> Option<Uuid> {
        match self {
            RememberRequest::Fact(f) => f.space_id,
            RememberRequest::Observation(o) => o.space_id,
            RememberRequest::Procedure(p) => p.space_id,
        }
    }

    pub fn validate(&self) -> Result<(), ApiError> {
        match self {
            RememberRequest::Fact(f) => {
                if !is_valid_predicate(&f.predicate) {
                    return Err(ApiError::invalid("predicate must be dotted lowercase (a-z0-9_)"));
                }
                char_range("subject.name", &f.subject.name, 1, 200)?;
                char_range("value_text", &f.value_text, 1, 300)
            }
            RememberRequest::Observation(o) => {
                char_range("title", &o.title, 1, 120)?;
                if let Some(s) = &o.subtitle {
                    char_range("subtitle", s, 0, 200)?;
                }
                if let Some(n) = &o.narrative {
                    char_range("narrative", n, 0, 2000)?;
                }
                if o.facts.len() > 10 || o.concepts.len() > 5 {
                    return Err(ApiError::invalid("at most 10 facts and 5 concepts"));
                }
                if !(0.0..=1.0).contains(&o.importance) {
                    return Err(ApiError::invalid("importance must be 0-1"));
                }
                Ok(())
            }
            RememberRequest::Procedure(p) => {
                for (name, v) in [("name", &p.name), ("task_class", &p.task_class), ("trigger_text", &p.trigger_text)] {
                    char_range(name, v, 1, 300)?;
                }
                char_range("body_md", &p.body_md, 1, 20_000)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RememberResponse {
    pub id: Uuid,
    pub table: MemoryTable,
    /// `active` for facts/procedures, `created` for observations.
    pub status: String,
    /// How it will be shown in context (index row or citation line).
    pub rendered: String,
}

/// `DELETE /v1/memories/{id}?hard=true&reason=...` (hard requires admin; the reason lands in the audit log).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetQuery {
    #[serde(default)]
    pub hard: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

impl ForgetQuery {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.reason.as_deref().is_some_and(|r| r.chars().count() > 500) {
            return Err(ApiError::invalid("reason must be at most 500 characters"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgetAction {
    Retracted,
    Archived,
    Retired,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForgetResponse {
    pub id: Uuid,
    pub table: MemoryTable,
    pub action: ForgetAction,
}
