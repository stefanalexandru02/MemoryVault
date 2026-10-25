//! Spec 6.7: admin API (admin scope).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::auth::scopes::TokenKind;
use crate::error::ApiError;
use crate::schemas::common::{ClientKind, EmbeddingOwnerTable, MembershipRole, SpaceKind};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserCreate {
    /// `^[a-z0-9][a-z0-9_.-]*$`, at most 64 chars.
    pub handle: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

impl UserCreate {
    pub fn validate(&self) -> Result<(), ApiError> {
        let ok = !self.handle.is_empty()
            && self.handle.len() <= 64
            && self.handle.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && self.handle.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b));
        if !ok {
            return Err(ApiError::invalid("handle must match ^[a-z0-9][a-z0-9_.-]*$ (max 64)"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct UserOut {
    pub id: Uuid,
    pub handle: String,
    pub display_name: String,
    pub default_space_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub disabled_at: Option<DateTime<Utc>>,
}

fn shared() -> SpaceKind {
    SpaceKind::Shared
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpaceCreate {
    pub name: String,
    #[serde(default = "shared")]
    pub kind: SpaceKind,
    pub owner_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct SpaceOut {
    pub id: Uuid,
    pub name: String,
    pub kind: SpaceKind,
    pub owner_id: Uuid,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct MembershipCreate {
    pub space_id: Uuid,
    pub user_id: Uuid,
    pub role: MembershipRole,
}

pub type MembershipOut = MembershipCreate;

fn device_kind() -> TokenKind {
    TokenKind::Device
}

/// Mint a token; the client `(user, device_id)` is created when missing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenCreate {
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Alternative to `user_id`.
    #[serde(default)]
    pub handle: Option<String>,
    pub device_id: String,
    /// Default from the token kind: device -> claude-code, runner -> runner, admin -> cli.
    #[serde(default)]
    pub client_kind: Option<ClientKind>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default = "device_kind")]
    pub kind: TokenKind,
    /// 1-3650.
    #[serde(default)]
    pub expires_days: Option<i64>,
}

impl TokenCreate {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.device_id.is_empty() {
            return Err(ApiError::invalid("device_id must not be empty"));
        }
        if self.user_id.is_none() && self.handle.is_none() {
            return Err(ApiError::invalid("user_id or handle is required"));
        }
        if self.expires_days.is_some_and(|d| !(1..=3650).contains(&d)) {
            return Err(ApiError::invalid("expires_days must be 1-3650"));
        }
        Ok(())
    }
}

/// The raw `token` is returned exactly once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenMinted {
    pub token: String,
    pub token_id: Uuid,
    pub prefix: String,
    pub user_id: Uuid,
    pub client_id: Uuid,
    pub kind: TokenKind,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct TokenOut {
    pub id: Uuid,
    pub user_id: Uuid,
    pub client_id: Uuid,
    pub kind: String,
    pub prefix: String,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConsolidateRequest {
    /// `None` = every space.
    #[serde(default)]
    pub space_id: Option<Uuid>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConsolidateResponse {
    pub run_id: Uuid,
    /// By packet kind.
    #[serde(default)]
    pub packets_created: BTreeMap<String, i64>,
    /// Code-only actions: auto-activate, archive, ...
    #[serde(default)]
    pub actions: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevertResponse {
    pub run_id: Uuid,
    pub reverted: i64,
    pub packets_cancelled: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct JobOut {
    pub id: Uuid,
    #[serde(rename = "type")]
    #[sqlx(rename = "type")]
    pub job_type: String,
    pub payload: serde_json::Value,
    pub status: String,
    pub attempts: i32,
    pub run_after: DateTime<Utc>,
    pub locked_by: Option<String>,
    pub locked_until: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacketRetryRequest {
    #[serde(default = "yes")]
    pub reset_attempts: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReindexRequest {
    /// Default: `settings.embed_model`.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tables: Option<Vec<EmbeddingOwnerTable>>,
    #[serde(default)]
    pub space_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReindexResponse {
    pub jobs_enqueued: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

impl DoctorStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DoctorStatus::Ok => "ok",
            DoctorStatus::Warn => "warn",
            DoctorStatus::Fail => "fail",
        }
    }
}

/// One line of `GET /v1/admin/doctor`: what was checked, how it came out and, unless it is fine, the
/// command or setting that fixes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: DoctorStatus,
    pub detail: String,
    #[serde(default)]
    pub fix: Option<String>,
}

impl DoctorCheck {
    pub fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status: DoctorStatus::Ok, detail: detail.into(), fix: None }
    }

    pub fn warn(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self { name: name.into(), status: DoctorStatus::Warn, detail: detail.into(), fix: Some(fix.into()) }
    }

    pub fn fail(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self { name: name.into(), status: DoctorStatus::Fail, detail: detail.into(), fix: Some(fix.into()) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorResponse {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorResponse {
    /// No check failed (warnings do not count).
    pub fn healthy(&self) -> bool {
        self.checks.iter().all(|c| c.status != DoctorStatus::Fail)
    }
}
