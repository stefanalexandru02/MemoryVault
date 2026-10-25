//! LLM output validation (spec 9.4): parse, strip one surrounding code fence, validate against the
//! JSON Schema `schemas/<name>.json`, then deserialize into the typed [`LlmOutput`]. Nothing else
//! is "fixed" heuristically. The error text is what `POST /v1/work/{id}/result` returns as
//! `retry_with` in its 422 `schema_invalid` response.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use axum::http::StatusCode;
use serde_json::Value;

use crate::error::ApiError;
use crate::schemas::common::OutputSchemaName;
use crate::schemas::outputs::LlmOutput;

/// At most this many schema violations are listed in the error text.
const MAX_ERRORS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutputError {
    #[error("unknown output schema {0:?}")]
    UnknownSchema(String),
    /// The text to return as `retry_with`.
    #[error("{0}")]
    Invalid(String),
}

impl From<OutputError> for ApiError {
    /// `Invalid` -> 422 `schema_invalid` with `retry_with`; `UnknownSchema` -> 500.
    fn from(e: OutputError) -> Self {
        match e {
            OutputError::Invalid(text) => {
                ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "schema_invalid", "output failed schema validation")
                    .with_extra("retry_with", text)
            }
            OutputError::UnknownSchema(name) => ApiError::internal(format!("unknown output schema {name:?}")),
        }
    }
}

/// The JSON Schemas of every output schema, compiled once at startup.
pub struct OutputSchemas {
    validators: BTreeMap<String, jsonschema::Validator>,
}

impl std::fmt::Debug for OutputSchemas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputSchemas").field("names", &self.validators.keys().collect::<Vec<_>>()).finish()
    }
}

/// Strip a single surrounding Markdown code fence (```` ```json ... ``` ````), else trim.
pub fn strip_code_fence(text: &str) -> &str {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("```")
        && let Some(inner) = rest.strip_suffix("```")
        && let Some((first_line, body)) = inner.split_once('\n')
        && first_line.trim().chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return body.trim();
    }
    t
}

impl OutputSchemas {
    /// Load every `*.json` in `dir` (name = file name without `.json`). Fails when a schema of
    /// [`OutputSchemaName`] is missing or a file is not a valid JSON Schema.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut validators = BTreeMap::new();
        let entries = std::fs::read_dir(dir).with_context(|| format!("reading SCHEMAS_DIR {}", dir.display()))?;
        for entry in entries {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let schema: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            let validator = jsonschema::validator_for(&schema)
                .map_err(|e| anyhow::anyhow!("invalid JSON Schema {}: {e}", path.display()))?;
            validators.insert(name.to_string(), validator);
        }
        for name in OutputSchemaName::ALL {
            anyhow::ensure!(
                validators.contains_key(name.as_str()),
                "schema {}.json missing from {}",
                name.as_str(),
                dir.display()
            );
        }
        Ok(Self { validators })
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.validators.keys().map(String::as_str)
    }

    /// Validate `output` (a JSON object, or the model's raw text as a JSON string) against the
    /// schema `name`; returns the parsed object.
    pub fn validate_value(&self, name: &str, output: &Value) -> Result<Value, OutputError> {
        let validator = self.validators.get(name).ok_or_else(|| OutputError::UnknownSchema(name.to_string()))?;
        let data = match output {
            Value::String(text) => serde_json::from_str(strip_code_fence(text))
                .map_err(|e| OutputError::Invalid(format!("output is not valid JSON: {e}")))?,
            other => other.clone(),
        };
        if !data.is_object() {
            return Err(OutputError::Invalid("output must be a JSON object".into()));
        }
        let errors: Vec<String> = validator
            .iter_errors(&data)
            .take(MAX_ERRORS)
            .map(|e| {
                let path = e.instance_path().to_string();
                if path.is_empty() { e.to_string() } else { format!("{path}: {e}") }
            })
            .collect();
        if !errors.is_empty() {
            return Err(OutputError::Invalid(format!("output does not match {name}: {}", errors.join("; "))));
        }
        Ok(data)
    }

    /// [`Self::validate_value`] plus deserialization into the typed output.
    pub fn validate(&self, name: OutputSchemaName, output: &Value) -> Result<LlmOutput, OutputError> {
        let data = self.validate_value(name.as_str(), output)?;
        LlmOutput::from_value(name, data)
            .map_err(|e| OutputError::Invalid(format!("output does not match {}: {e}", name.as_str())))
    }
}
