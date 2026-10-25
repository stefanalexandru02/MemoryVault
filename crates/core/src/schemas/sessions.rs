//! Spec 6.1: sessions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;

fn default_harness() -> String {
    "claude-code".into()
}

/// `POST /v1/sessions` (events:write).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCreate {
    #[serde(default = "default_harness")]
    pub harness: String,
    pub external_session_id: String,
    #[serde(default)]
    pub project_key: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub space_id: Option<Uuid>,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl SessionCreate {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.external_session_id.is_empty() || self.external_session_id.len() > 256 {
            return Err(ApiError::invalid("external_session_id must be 1-256 characters"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPolicy {
    pub tool_uses: i32,
    pub idle_seconds: i64,
}

impl Default for WindowPolicy {
    fn default() -> Self {
        Self { tool_uses: 8, idle_seconds: 90 }
    }
}

/// 201 on create, 200 with the existing id on a repeat of `(client, external_session_id)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCreated {
    pub session_id: Uuid,
    pub space_id: Uuid,
    #[serde(default)]
    pub window_policy: WindowPolicy,
    /// One past the highest event `seq` stored for the session (0 for a new one): a client that
    /// lost its counter continues from here instead of colliding with stored events.
    #[serde(default)]
    pub next_seq: i32,
}

/// `POST /v1/sessions/{id}/end` (events:write).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionEnd {
    #[serde(default)]
    pub reason: Option<String>,
    /// When the session ended, if the client delivers late (a spooled end). Clamped to
    /// `started_at ..= now`. Default: now.
    #[serde(default)]
    pub occurred_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEnded {
    pub session_id: Uuid,
    pub ended_at: DateTime<Utc>,
    /// The open window was closed (an extract packet was created if it had content).
    pub window_closed: bool,
    /// Packet kinds created by this call.
    #[serde(default)]
    pub packets_created: Vec<String>,
}
