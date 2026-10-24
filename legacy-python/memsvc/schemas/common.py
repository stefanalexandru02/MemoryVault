"""Shared literals and small models used across the API (spec 4, 6)."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, ConfigDict

ScopeKind = Literal["global", "project", "device", "agent", "session"]
Sensitivity = Literal["normal", "sensitive", "secret"]
EventKind = Literal[
    "user_prompt", "assistant_message", "tool_use", "tool_result", "tool_failure", "subagent_stop", "system", "note"
]
PacketKind = Literal[
    "extract",
    "summarize_prompt",
    "summarize_session",
    "consolidate_facts",
    "consolidate_summary",
    "consolidate_procedures",
]
PacketStatus = Literal["pending", "leased", "done", "failed", "cancelled"]
OutputSchemaName = Literal[
    "observer.v1", "summary.v1", "consolidate.facts.v1", "consolidate.summary.v1", "consolidate.procedures.v1"
]
MemoryTable = Literal["observations", "summaries", "facts", "procedures"]
EmbeddingOwnerTable = Literal["observations", "summaries", "facts", "procedures", "entities"]
SummaryGrain = Literal["prompt", "session", "day", "week"]
FactStatus = Literal["candidate", "active", "superseded", "retracted"]
MembershipRole = Literal["owner", "writer", "reader"]
SpaceKind = Literal["private", "shared"]
ClientKind = Literal["claude-code", "agentic-os", "runner", "cli"]

# Code mode only (spec 10.2): claude-mem's nine observation types.
ObservationType = Literal[
    "bugfix", "feature", "refactor", "change", "discovery", "decision", "security_alert", "security_note", "sensitive"
]
OBSERVATION_TYPES: tuple[str, ...] = ObservationType.__args__  # type: ignore[attr-defined]
Concept = Literal["how-it-works", "why-it-exists", "what-changed", "problem-solution", "gotcha", "pattern", "trade-off"]
CONCEPTS: tuple[str, ...] = Concept.__args__  # type: ignore[attr-defined]


class Scope(BaseModel):
    """A scope is ``(kind, key)``; global uses key ``""``. Text form ``kind:key`` (query params, cache keys)."""

    model_config = ConfigDict(frozen=True)

    kind: ScopeKind
    key: str = ""

    def as_text(self) -> str:
        return f"{self.kind}:{self.key}"

    @classmethod
    def parse(cls, text: str) -> Scope:
        kind, _, key = text.partition(":")
        return cls(kind=kind, key=key)  # type: ignore[arg-type]


def parse_scopes_param(value: str | None) -> list[Scope] | None:
    """Parse a ``scopes=kind:key,kind:key`` query parameter."""
    if not value:
        return None
    return [Scope.parse(part) for part in value.split(",") if part]


class ErrorDetail(BaseModel):
    model_config = ConfigDict(extra="allow")

    code: str
    message: str
    retryable: bool = False
    retry_with: str | None = None  # schema_invalid only: validation text to append to the prompt (6.6)


class ErrorResponse(BaseModel):
    """Every non-2xx JSON body: ``{"error": {"code", "message", "retryable"}}``."""

    error: ErrorDetail


class OkResponse(BaseModel):
    ok: bool = True


class HealthTEI(BaseModel):
    embed: bool
    rerank: bool


class HealthResponse(BaseModel):
    status: Literal["ok", "degraded", "down"]
    db: bool
    tei: HealthTEI
    version: str
    pg_search: bool | None = None


def estimate_tokens(text: str | None) -> int:
    """Project-wide token estimate: ``ceil(len(text) / 4)`` (replaces tiktoken, see CONTRACTS.md)."""
    return (len(text) + 3) // 4 if text else 0
