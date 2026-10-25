//! Token scopes and kinds (spec 5.2).

use chrono::TimeDelta;
use serde::{Deserialize, Serialize};

use crate::schemas::common::ClientKind;

/// `POST /v1/sessions`, `/v1/sessions/{id}/events`, `/v1/sessions/{id}/end`.
pub const EVENTS_WRITE: &str = "events:write";
/// Recall, context, search, timeline, get, explain, work-state read, MCP read tools.
pub const MEMORIES_READ: &str = "memories:read";
/// `memory_remember`, `memory_forget` (soft), `work_state_write`, feedback.
pub const MEMORIES_WRITE: &str = "memories:write";
/// `POST /v1/work/lease`, `/v1/work/{id}/result|fail|heartbeat`.
pub const WORK_LEASE: &str = "work:lease";
/// Token mint/revoke, users/spaces, hard delete, consolidation trigger, revert.
pub const ADMIN: &str = "admin";

pub const ALL_SCOPES: [&str; 5] = [EVENTS_WRITE, MEMORIES_READ, MEMORIES_WRITE, WORK_LEASE, ADMIN];

/// Token kinds minted by the admin CLI/API. `agent` is dropped (no agentic OS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    /// Claude Code machine.
    Device,
    /// Nightly consolidation runner.
    Runner,
    /// Never installed on a client.
    Admin,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Device => "device",
            TokenKind::Runner => "runner",
            TokenKind::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "device" => Some(TokenKind::Device),
            "runner" => Some(TokenKind::Runner),
            "admin" => Some(TokenKind::Admin),
            _ => None,
        }
    }

    pub fn scopes(self) -> &'static [&'static str] {
        match self {
            TokenKind::Device => &[EVENTS_WRITE, MEMORIES_READ, MEMORIES_WRITE, WORK_LEASE],
            TokenKind::Runner => &[WORK_LEASE, MEMORIES_READ],
            TokenKind::Admin => &[ADMIN, EVENTS_WRITE, MEMORIES_READ, MEMORIES_WRITE, WORK_LEASE],
        }
    }

    /// Device 365 days, runner 90, admin 30.
    pub fn ttl(self) -> TimeDelta {
        match self {
            TokenKind::Device => TimeDelta::days(365),
            TokenKind::Runner => TimeDelta::days(90),
            TokenKind::Admin => TimeDelta::days(30),
        }
    }

    /// Default `clients.kind` for a client created alongside a token of this kind.
    pub fn client_kind(self) -> ClientKind {
        match self {
            TokenKind::Device => ClientKind::ClaudeCode,
            TokenKind::Runner => ClientKind::Runner,
            TokenKind::Admin => ClientKind::Cli,
        }
    }
}

impl std::fmt::Display for TokenKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
