//! Spec 10.1: prompt registry entries (`prompts/*.yaml`).

use serde::{Deserialize, Serialize};

use crate::schemas::common::OutputSchemaName;

fn default_model_hint() -> String {
    "sonnet".into()
}

/// One versioned prompt. `prompt_ref = name@version`; packets pin the ref they were built with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSpec {
    /// e.g. `observer.code` (`[a-z0-9_.-]+`).
    pub name: String,
    /// e.g. `v3` (`v<digits>`).
    pub version: String,
    pub system: String,
    /// minijinja template rendered with the packet inputs (strict undefined).
    pub user_template: String,
    pub output_schema: OutputSchemaName,
    #[serde(default = "default_model_hint")]
    pub model_hint: String,
    #[serde(default)]
    pub description: Option<String>,
}

impl PromptSpec {
    pub fn prompt_ref(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// The numeric part of `version` (`v3` -> 3).
    pub fn version_number(&self) -> Option<u32> {
        self.version.strip_prefix('v')?.parse().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedPrompt {
    pub system: String,
    pub user: String,
}

/// `GET /v1/prompts/{name}` (memories:read).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptOut {
    pub name: String,
    pub version: String,
    pub prompt_ref: String,
    pub system: String,
    pub user_template: String,
    pub output_schema: OutputSchemaName,
    pub model_hint: String,
    /// Every loaded version of this name, oldest first.
    #[serde(default)]
    pub versions: Vec<String>,
}
