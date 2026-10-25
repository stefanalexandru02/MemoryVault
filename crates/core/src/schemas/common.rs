//! Shared enums and small types used across the API (spec 4, 6).
//!
//! The string enums declared with `str_enum!` serialize as their spec spelling (`"tool_use"`),
//! implement `as_str`/`FromStr`/`Display`, and bind to / decode from Postgres `text` columns, so
//! `.bind(PacketKind::Extract)` and `row.try_get::<PacketKind, _>("kind")` work. [`ScopeKind`] maps to the
//! Postgres enum type `scope_kind`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

/// `FromStr` error of the string enums.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown {type_name} value {value:?}")]
pub struct UnknownVariant {
    pub type_name: &'static str,
    pub value: String,
}

macro_rules! str_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $s)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = UnknownVariant;
            fn from_str(s: &str) -> Result<Self, UnknownVariant> {
                match s {
                    $($s => Ok($name::$variant),)+
                    _ => Err(UnknownVariant { type_name: stringify!($name), value: s.to_string() }),
                }
            }
        }

        impl sqlx::Type<sqlx::Postgres> for $name {
            fn type_info() -> sqlx::postgres::PgTypeInfo {
                <&str as sqlx::Type<sqlx::Postgres>>::type_info()
            }
            fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
                <&str as sqlx::Type<sqlx::Postgres>>::compatible(ty)
            }
        }

        impl sqlx::postgres::PgHasArrayType for $name {
            fn array_type_info() -> sqlx::postgres::PgTypeInfo {
                <&str as sqlx::postgres::PgHasArrayType>::array_type_info()
            }
            fn array_compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
                <&str as sqlx::postgres::PgHasArrayType>::array_compatible(ty)
            }
        }

        impl<'q> sqlx::Encode<'q, sqlx::Postgres> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut sqlx::postgres::PgArgumentBuffer,
            ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
                <&str as sqlx::Encode<'q, sqlx::Postgres>>::encode_by_ref(&self.as_str(), buf)
            }
        }

        impl<'r> sqlx::Decode<'r, sqlx::Postgres> for $name {
            fn decode(value: sqlx::postgres::PgValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
                let s = <&str as sqlx::Decode<'r, sqlx::Postgres>>::decode(value)?;
                Ok(s.parse()?)
            }
        }
    };
}

/// Postgres enum `scope_kind` (spec 4.2). Binds as `scope_kind`; `Vec<ScopeKind>` as `scope_kind[]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "scope_kind", rename_all = "lowercase")]
pub enum ScopeKind {
    Global,
    Project,
    Device,
    Agent,
    Session,
}

impl ScopeKind {
    pub const ALL: &'static [ScopeKind] =
        &[ScopeKind::Global, ScopeKind::Project, ScopeKind::Device, ScopeKind::Agent, ScopeKind::Session];

    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Global => "global",
            ScopeKind::Project => "project",
            ScopeKind::Device => "device",
            ScopeKind::Agent => "agent",
            ScopeKind::Session => "session",
        }
    }
}

impl fmt::Display for ScopeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ScopeKind {
    type Err = UnknownVariant;
    fn from_str(s: &str) -> Result<Self, UnknownVariant> {
        Self::ALL
            .iter()
            .copied()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| UnknownVariant { type_name: "ScopeKind", value: s.to_string() })
    }
}

/// A scope is `(kind, key)`; global uses key `""`. Text form `kind:key` (query params, cache keys, NOTIFY).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Scope {
    pub kind: ScopeKind,
    #[serde(default)]
    pub key: String,
}

impl Scope {
    pub fn new(kind: ScopeKind, key: impl Into<String>) -> Self {
        Self { kind, key: key.into() }
    }

    pub fn global() -> Self {
        Self::new(ScopeKind::Global, "")
    }

    pub fn as_text(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.key)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.as_str(), self.key)
    }
}

impl FromStr for Scope {
    type Err = UnknownVariant;
    /// `kind:key`; the key may contain ':'; `global` alone is accepted.
    fn from_str(s: &str) -> Result<Self, UnknownVariant> {
        let (kind, key) = s.split_once(':').unwrap_or((s, ""));
        Ok(Scope::new(kind.parse()?, key))
    }
}

/// Parse a `scopes=kind:key,kind:key` query parameter.
pub fn parse_scopes_param(value: &str) -> Result<Vec<Scope>, UnknownVariant> {
    value.split(',').filter(|p| !p.is_empty()).map(str::parse).collect()
}

str_enum!(Sensitivity { Normal = "normal", Sensitive = "sensitive", Secret = "secret" });

str_enum!(
    /// How `GET /v1/context` injects memory at session start: `full` is the budgeted block, `lean` only the
    /// profile, open work and a rubric on when to call the pull tools.
    #[derive(Default)]
    InjectionMode {
        #[default]
        Full = "full",
        Lean = "lean",
    }
);

str_enum!(
    /// What started a planner run: the nightly pass, the end of a session, enough new observations in a scope, or
    /// an operator (`POST /v1/admin/consolidate`, `memvault admin consolidate now`).
    PlanTrigger {
        Nightly = "nightly",
        SessionEnd = "session_end",
        Threshold = "threshold",
        Manual = "manual",
    }
);

str_enum!(EventKind {
    UserPrompt = "user_prompt",
    AssistantMessage = "assistant_message",
    ToolUse = "tool_use",
    ToolResult = "tool_result",
    ToolFailure = "tool_failure",
    SubagentStop = "subagent_stop",
    System = "system",
    Note = "note",
});

str_enum!(PacketKind {
    Extract = "extract",
    SummarizePrompt = "summarize_prompt",
    SummarizeSession = "summarize_session",
    ConsolidateFacts = "consolidate_facts",
    ConsolidateSummary = "consolidate_summary",
    ConsolidateProcedures = "consolidate_procedures",
});

str_enum!(PacketStatus {
    Pending = "pending",
    Leased = "leased",
    Done = "done",
    Failed = "failed",
    Cancelled = "cancelled",
});

str_enum!(
    /// `work_packets.output_schema`; each names a file `schemas/<name>.json`.
    OutputSchemaName {
        ObserverV1 = "observer.v1",
        SummaryV1 = "summary.v1",
        ConsolidateFactsV1 = "consolidate.facts.v1",
        ConsolidateSummaryV1 = "consolidate.summary.v1",
        ConsolidateProceduresV1 = "consolidate.procedures.v1",
    }
);

str_enum!(MemoryTable {
    Observations = "observations",
    Summaries = "summaries",
    Facts = "facts",
    Procedures = "procedures",
});

str_enum!(EmbeddingOwnerTable {
    Observations = "observations",
    Summaries = "summaries",
    Facts = "facts",
    Procedures = "procedures",
    Entities = "entities",
});

str_enum!(SummaryGrain { Prompt = "prompt", Session = "session", Day = "day", Week = "week" });

str_enum!(FactStatus {
    Candidate = "candidate",
    Active = "active",
    Superseded = "superseded",
    Retracted = "retracted",
});

str_enum!(FeedbackVerdict { Helpful = "helpful", Wrong = "wrong", Outdated = "outdated" });

str_enum!(MembershipRole { Owner = "owner", Writer = "writer", Reader = "reader" });

str_enum!(SpaceKind { Private = "private", Shared = "shared" });

str_enum!(ClientKind { ClaudeCode = "claude-code", AgenticOs = "agentic-os", Runner = "runner", Cli = "cli" });

str_enum!(
    /// Code mode (spec 10.2): claude-mem's nine observation types.
    ObservationType {
        Bugfix = "bugfix",
        Feature = "feature",
        Refactor = "refactor",
        Change = "change",
        Discovery = "discovery",
        Decision = "decision",
        SecurityAlert = "security_alert",
        SecurityNote = "security_note",
        Sensitive = "sensitive",
    }
);

str_enum!(Concept {
    HowItWorks = "how-it-works",
    WhyItExists = "why-it-exists",
    WhatChanged = "what-changed",
    ProblemSolution = "problem-solution",
    Gotcha = "gotcha",
    Pattern = "pattern",
    TradeOff = "trade-off",
});

str_enum!(
    /// `scope_hint` in LLM outputs (spec 10.3).
    ScopeHint { Global = "global", Project = "project", Device = "device", Agent = "agent" }
);

impl From<ScopeHint> for ScopeKind {
    fn from(h: ScopeHint) -> Self {
        match h {
            ScopeHint::Global => ScopeKind::Global,
            ScopeHint::Project => ScopeKind::Project,
            ScopeHint::Device => ScopeKind::Device,
            ScopeHint::Agent => ScopeKind::Agent,
        }
    }
}

/// Every non-2xx JSON body: `{"error": {"code", "message", "retryable", ...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    /// `schema_invalid` only: validation text to append to the prompt (6.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_with: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthTei {
    pub embed: bool,
    pub rerank: bool,
}

/// `GET /health`: `ok`, `degraded` (TEI down) or `down` (503, DB down).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub db: bool,
    pub tei: HealthTei,
    pub version: String,
    pub pg_search: Option<bool>,
}

/// Project-wide token estimate: `ceil(chars / 4)` (replaces tiktoken, spec 2).
pub fn estimate_tokens(text: &str) -> i32 {
    i32::try_from(text.chars().count().div_ceil(4)).unwrap_or(i32::MAX)
}

/// Serde helpers for list query parameters given comma-separated (`types=decision,bugfix`) or as a
/// JSON array. Use with `#[serde(default, deserialize_with = "comma_list::deserialize")]` on an
/// `Option<Vec<T>>` where `T: FromStr`.
pub mod comma_list {
    use super::*;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        One(String),
        Many(Vec<String>),
    }

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Vec<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: fmt::Display,
    {
        let parts: Vec<String> = match Option::<Raw>::deserialize(d)? {
            None => return Ok(None),
            Some(Raw::One(s)) => s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect(),
            Some(Raw::Many(v)) => v,
        };
        if parts.is_empty() {
            return Ok(None);
        }
        parts.iter().map(|p| p.parse().map_err(serde::de::Error::custom)).collect::<Result<Vec<T>, _>>().map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_helpers() {
        assert_eq!("project:github.com/a/b".parse::<Scope>().unwrap().key, "github.com/a/b");
        assert_eq!(Scope::global().as_text(), "global:");
        assert_eq!(
            parse_scopes_param("global:,device:laptop").unwrap(),
            vec![Scope::global(), Scope::new(ScopeKind::Device, "laptop")]
        );
        assert!(parse_scopes_param("nope:x").is_err());
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("ééé"), 1);
    }

    #[test]
    fn str_enums() {
        assert_eq!(serde_json::to_value(EventKind::ToolUse).unwrap(), "tool_use");
        assert_eq!("consolidate.facts.v1".parse::<OutputSchemaName>().unwrap(), OutputSchemaName::ConsolidateFactsV1);
        assert_eq!(ClientKind::ClaudeCode.to_string(), "claude-code");
        assert!("opinion".parse::<ObservationType>().is_err());
        assert_eq!(serde_json::to_value(ScopeKind::Global).unwrap(), "global");
    }
}

/// Serde helper for an optional query parameter parsed with `FromStr` (e.g. `scope=project:x`).
/// Use with `#[serde(default, deserialize_with = "opt_from_str::deserialize")]`.
pub mod opt_from_str {
    use super::*;

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: fmt::Display,
    {
        match Option::<String>::deserialize(d)? {
            None => Ok(None),
            Some(s) if s.is_empty() => Ok(None),
            Some(s) => s.parse().map(Some).map_err(serde::de::Error::custom),
        }
    }
}
