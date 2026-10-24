//! Spec 9.2: packet `inputs` for extract / summarize packets (immutable at creation).
//!
//! Prompt templates (`prompts/*.yaml`) render these fields. Consolidation packet inputs are
//! defined by memvault-consolidate (spec 12) and stored as plain JSON.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

fn code_mode() -> String {
    "code".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacketSessionInfo {
    pub harness: String,
    #[serde(default)]
    pub project_key: Option<String>,
    /// `clients.device_id`.
    #[serde(default)]
    pub device: Option<String>,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacketEvent {
    pub seq: i32,
    pub kind: String,
    pub at: DateTime<Utc>,
    /// tool_use / tool_failure.
    #[serde(default)]
    pub tool: Option<String>,
    /// `tool_input`.
    #[serde(default)]
    pub input: serde_json::Value,
    /// `tool_response`, or the error for tool_failure.
    #[serde(default)]
    pub output: serde_json::Value,
    /// user_prompt text, assistant/subagent last message, system/note text.
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentTitle {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub obs_type: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractInputs {
    #[serde(default = "code_mode")]
    pub mode: String,
    pub session: PacketSessionInfo,
    pub window_no: i32,
    pub events: Vec<PacketEvent>,
    /// Last 20 observations in this session, any window.
    #[serde(default)]
    pub recent_titles: Vec<RecentTitle>,
    /// Project-scope profile block, <= 400 tokens.
    #[serde(default)]
    pub profile_md: String,
    #[serde(default)]
    pub known_entities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummarizePromptInputs {
    #[serde(default = "code_mode")]
    pub mode: String,
    pub session: PacketSessionInfo,
    pub prompt_no: i32,
    /// The user prompt text.
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub observation_titles: Vec<RecentTitle>,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptSummaryIn {
    #[serde(default)]
    pub prompt_no: Option<i32>,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummarizeSessionInputs {
    #[serde(default = "code_mode")]
    pub mode: String,
    pub session: PacketSessionInfo,
    pub prompt_summaries: Vec<PromptSummaryIn>,
}
