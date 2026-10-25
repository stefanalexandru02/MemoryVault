//! Spec 7: MCP tool arguments. Results are markdown text (budgeted < 8k tokens, 40k chars).

use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::schemas::memories::MAX_GET_IDS;

/// `_meta["anthropic/maxResultSizeChars"]` on every tool.
pub const MAX_RESULT_CHARS: usize = 40_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpScope {
    Project,
    Global,
    #[default]
    All,
}

fn twenty() -> i64 {
    20
}

fn five() -> i64 {
    5
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchArgs {
    pub query: String,
    #[serde(default)]
    pub types: Option<Vec<String>>,
    /// Default: the project when the request carries the project header, else all.
    #[serde(default)]
    pub scope: Option<McpScope>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub files: Option<Vec<String>>,
    /// RFC 3339 time: search the memory as it stood then.
    #[serde(default)]
    pub as_of: Option<String>,
    /// <= 30.
    #[serde(default = "twenty")]
    pub limit: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryTimelineArgs {
    pub anchor_id: String,
    /// <= 10.
    #[serde(default = "five")]
    pub before: i64,
    /// <= 10.
    #[serde(default = "five")]
    pub after: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryGetArgs {
    /// <= 20.
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryExplainArgs {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHistoryArgs {
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RememberKind {
    Fact,
    Note,
    Procedure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RememberScope {
    Project,
    Global,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRememberArgs {
    pub kind: RememberKind,
    pub content: String,
    #[serde(default)]
    pub scope: Option<RememberScope>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub predicate: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryForgetArgs {
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkStateWriteArgs {
    pub list: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl MemorySearchArgs {
    pub fn validate(&self) -> Result<(), ApiError> {
        if !(1..=30).contains(&self.limit) {
            return Err(ApiError::invalid("limit must be 1-30"));
        }
        Ok(())
    }
}

impl MemoryTimelineArgs {
    pub fn validate(&self) -> Result<(), ApiError> {
        if !(0..=10).contains(&self.before) || !(0..=10).contains(&self.after) {
            return Err(ApiError::invalid("before and after must be 0-10"));
        }
        Ok(())
    }
}

impl MemoryGetArgs {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.ids.is_empty() || self.ids.len() > MAX_GET_IDS {
            return Err(ApiError::invalid(format!("ids must hold 1-{MAX_GET_IDS} items")));
        }
        Ok(())
    }
}

impl MemoryRememberArgs {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.content.trim().is_empty() {
            return Err(ApiError::invalid("content must not be empty"));
        }
        Ok(())
    }
}

impl WorkStateWriteArgs {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.list.is_empty() || self.fields.is_empty() {
            return Err(ApiError::invalid("list and fields must not be empty"));
        }
        Ok(())
    }
}
