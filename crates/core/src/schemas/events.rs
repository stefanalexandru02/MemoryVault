//! Spec 6.2: event batches.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::schemas::common::EventKind;

pub const MAX_EVENTS_PER_BATCH: usize = 200;
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Per string field inside `payload`: keep the head, a `…[clamped N chars]…` marker, and the tail.
pub const FIELD_HEAD_CHARS: usize = 6144;
pub const FIELD_TAIL_CHARS: usize = 2048;
/// Whole payload hard cap after clamping.
pub const PAYLOAD_CAP_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventIn {
    pub seq: i32,
    pub kind: EventKind,
    pub occurred_at: DateTime<Utc>,
    pub idempotency_key: String,
    #[serde(default)]
    pub payload: serde_json::Map<String, serde_json::Value>,
}

/// `POST /v1/sessions/{id}/events` (events:write).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventsBatch {
    pub events: Vec<EventIn>,
}

impl EventsBatch {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.events.is_empty() || self.events.len() > MAX_EVENTS_PER_BATCH {
            return Err(ApiError::invalid(format!("events must hold 1-{MAX_EVENTS_PER_BATCH} items")));
        }
        for e in &self.events {
            if e.seq < 0 {
                return Err(ApiError::invalid("seq must be >= 0"));
            }
            if e.idempotency_key.is_empty() || e.idempotency_key.len() > 256 {
                return Err(ApiError::invalid("idempotency_key must be 1-256 characters"));
            }
        }
        Ok(())
    }
}

/// 202 response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventsAccepted {
    pub accepted: i64,
    pub duplicates: i64,
    pub window_no: i32,
    pub window_closed: bool,
}
