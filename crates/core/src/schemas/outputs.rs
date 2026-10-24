//! Typed LLM outputs named by `work_packets.output_schema` (spec 10.3, 10.5, 12.2, 12.3), code mode.
//!
//! These are deserialized only after the JSON Schema in `schemas/<name>.json` accepted the output
//! (see [`crate::schemas::validate`]). Unknown keys are ignored.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::schemas::common::{Concept, ObservationType, OutputSchemaName, ScopeHint, Sensitivity};

// ---------- observer.v1 (10.3) ----------

fn normal() -> Sensitivity {
    Sensitivity::Normal
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedObservation {
    #[serde(rename = "type")]
    pub obs_type: ObservationType,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub narrative: Option<String>,
    pub facts: Vec<String>,
    pub concepts: Vec<Concept>,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    pub importance: f64,
    #[serde(default = "normal")]
    pub sensitivity: Sensitivity,
    pub source_event_seqs: Vec<i64>,
    /// Not in the 10.3 schema; 10.4 lets the prompt choose device scope for machine-level notes.
    #[serde(default)]
    pub scope_hint: Option<ScopeHint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactSubjectOut {
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactCandidate {
    pub subject: FactSubjectOut,
    pub predicate: String,
    pub value: serde_json::Value,
    pub value_text: String,
    pub confidence: f64,
    #[serde(default)]
    pub scope_hint: Option<ScopeHint>,
    #[serde(default)]
    pub valid_from: Option<String>,
    pub evidence_seqs: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureHint {
    pub task_class: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObserverV1 {
    pub skip: bool,
    #[serde(default)]
    pub skip_reason: Option<String>,
    pub observations: Vec<ObservedObservation>,
    pub fact_candidates: Vec<FactCandidate>,
    #[serde(default)]
    pub procedure_hints: Vec<ProcedureHint>,
    #[serde(default)]
    pub needs_escalation: bool,
}

// ---------- summary.v1 (10.5) ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryV1 {
    #[serde(default)]
    pub request: Option<String>,
    #[serde(default)]
    pub investigated: Option<String>,
    #[serde(default)]
    pub learned: Option<String>,
    #[serde(default)]
    pub completed: Option<String>,
    /// Mandatory, phrased as the current trajectory.
    pub next_steps: String,
    #[serde(default)]
    pub notes: Option<String>,
}

// ---------- consolidate.facts.v1 (12.2) ----------
// Ids are strings exactly as given in the packet inputs (full UUIDs or the packet's own aliases).

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FactDecision {
    Supersede {
        target_id: String,
        by_candidate_id: String,
        #[serde(default)]
        valid_from: Option<String>,
        reason: String,
    },
    MergeInto {
        target_id: String,
        candidate_id: String,
        reason: String,
    },
    KeepBoth {
        ids: Vec<String>,
        reason: String,
    },
    Retract {
        target_id: String,
        reason: String,
    },
    PromoteScope {
        target_id: String,
        to: ScopeHint,
        reason: String,
    },
    SetConfidence {
        target_id: String,
        confidence: f64,
        reason: String,
    },
}

impl FactDecision {
    pub fn reason(&self) -> &str {
        match self {
            FactDecision::Supersede { reason, .. }
            | FactDecision::MergeInto { reason, .. }
            | FactDecision::KeepBoth { reason, .. }
            | FactDecision::Retract { reason, .. }
            | FactDecision::PromoteScope { reason, .. }
            | FactDecision::SetConfidence { reason, .. } => reason,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsolidateFactsV1 {
    pub decisions: Vec<FactDecision>,
}

// ---------- consolidate.summary.v1 / consolidate.procedures.v1 (12.3) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeriodGrain {
    Day,
    Week,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Period {
    #[serde(default)]
    pub grain: Option<PeriodGrain>,
    pub start: NaiveDate,
    pub end: NaiveDate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsolidateSummaryV1 {
    #[serde(flatten)]
    pub summary: SummaryV1,
    pub period: Period,
}

fn default_confidence() -> f64 {
    0.6
}

/// One procedure (new, or a revision when `supersedes_id` is set). `skip` (not in the spec) lets
/// the model decline; without it `name`, `task_class`, `trigger_text` and `body_md` are present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsolidateProceduresV1 {
    #[serde(default)]
    pub skip: bool,
    #[serde(default)]
    pub skip_reason: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub task_class: Option<String>,
    #[serde(default)]
    pub trigger_text: Option<String>,
    #[serde(default)]
    pub body_md: Option<String>,
    #[serde(default)]
    pub supersedes_id: Option<String>,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
}

/// A validated LLM output, by schema.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum LlmOutput {
    Observer(ObserverV1),
    Summary(SummaryV1),
    ConsolidateFacts(ConsolidateFactsV1),
    ConsolidateSummary(ConsolidateSummaryV1),
    ConsolidateProcedures(ConsolidateProceduresV1),
}

impl LlmOutput {
    pub fn schema(&self) -> OutputSchemaName {
        match self {
            LlmOutput::Observer(_) => OutputSchemaName::ObserverV1,
            LlmOutput::Summary(_) => OutputSchemaName::SummaryV1,
            LlmOutput::ConsolidateFacts(_) => OutputSchemaName::ConsolidateFactsV1,
            LlmOutput::ConsolidateSummary(_) => OutputSchemaName::ConsolidateSummaryV1,
            LlmOutput::ConsolidateProcedures(_) => OutputSchemaName::ConsolidateProceduresV1,
        }
    }

    /// Deserialize a (schema-validated) JSON object into the typed output for `schema`.
    pub fn from_value(schema: OutputSchemaName, value: serde_json::Value) -> Result<Self, serde_json::Error> {
        Ok(match schema {
            OutputSchemaName::ObserverV1 => LlmOutput::Observer(serde_json::from_value(value)?),
            OutputSchemaName::SummaryV1 => LlmOutput::Summary(serde_json::from_value(value)?),
            OutputSchemaName::ConsolidateFactsV1 => LlmOutput::ConsolidateFacts(serde_json::from_value(value)?),
            OutputSchemaName::ConsolidateSummaryV1 => LlmOutput::ConsolidateSummary(serde_json::from_value(value)?),
            OutputSchemaName::ConsolidateProceduresV1 => {
                LlmOutput::ConsolidateProcedures(serde_json::from_value(value)?)
            }
        })
    }
}
