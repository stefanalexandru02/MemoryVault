"""Spec 6.4 (memories), 12.5 (explain)."""

from __future__ import annotations

from datetime import datetime
from typing import Annotated, Any, Literal
from uuid import UUID

from pydantic import BaseModel, Field, model_validator

from memsvc.schemas.common import (
    Concept,
    FactStatus,
    MemoryTable,
    ObservationType,
    Scope,
    Sensitivity,
    SummaryGrain,
)

MAX_GET_IDS = 20


# ---------- search / timeline / by-file: index rows ----------


class SearchQuery(BaseModel):
    """``GET /v1/search`` query params (list params are comma-separated in the URL).

    Requires ``q`` or at least one filter (types, since, until, files, scopes).
    """

    q: str | None = None
    space_ids: list[UUID] | None = None
    scopes: list[Scope] | None = None
    types: list[str] | None = None
    tables: list[MemoryTable] | None = None  # default: observations and summaries
    since: datetime | None = None
    until: datetime | None = None
    files: list[str] | None = None
    limit: int = Field(20, ge=1, le=100)
    offset: int = Field(0, ge=0)

    @model_validator(mode="after")
    def _needs_query_or_filter(self) -> SearchQuery:
        if not (self.q and self.q.strip()) and not (
            self.types or self.since or self.until or self.files or self.scopes
        ):
            raise ValueError("search requires q or at least one filter")
        return self


class IndexRow(BaseModel):
    id: UUID
    table: MemoryTable
    type: str | None = None  # observation type, summary grain, fact predicate, procedure task_class
    title: str
    created_at: datetime
    scope: Scope
    token_estimate: int = 0
    score: float | None = None
    is_anchor: bool = False  # timeline only


class SearchResponse(BaseModel):
    rows: list[IndexRow]
    limit: int
    offset: int


class TimelineQuery(BaseModel):
    anchor_id: UUID
    before: int = Field(5, ge=0, le=50)
    after: int = Field(5, ge=0, le=50)


class TimelineResponse(BaseModel):
    anchor_id: UUID
    rows: list[IndexRow]  # time order, anchor row has is_anchor = true


class ByFileQuery(BaseModel):
    path: str
    project_key: str | None = None
    limit: int = Field(15, ge=1, le=50)


# ---------- full bodies ----------


class EntityRef(BaseModel):
    id: UUID
    kind: str
    canonical_name: str


class ObservationFull(BaseModel):
    table: Literal["observations"] = "observations"
    id: UUID
    space_id: UUID
    scope: Scope
    session_id: UUID | None = None
    window_no: int | None = None
    type: str
    title: str
    subtitle: str | None = None
    narrative: str | None = None
    facts: list[str] = Field(default_factory=list)
    concepts: list[str] = Field(default_factory=list)
    entities: list[str] = Field(default_factory=list)
    files: list[str] = Field(default_factory=list)
    importance: float
    sensitivity: Sensitivity = "normal"
    source_event_ids: list[UUID] = Field(default_factory=list)
    extracted_by: dict[str, Any] = Field(default_factory=dict)
    written_by_client_id: UUID | None = None
    token_estimate: int = 0
    created_at: datetime
    archived_at: datetime | None = None
    superseded_by: UUID | None = None


class SummaryFull(BaseModel):
    table: Literal["summaries"] = "summaries"
    id: UUID
    space_id: UUID
    scope: Scope
    grain: SummaryGrain
    session_id: UUID | None = None
    prompt_no: int | None = None
    period_start: datetime | None = None
    period_end: datetime | None = None
    request: str | None = None
    investigated: str | None = None
    learned: str | None = None
    completed: str | None = None
    next_steps: str | None = None
    notes: str | None = None
    extracted_by: dict[str, Any] = Field(default_factory=dict)
    token_estimate: int = 0
    created_at: datetime


class FactFull(BaseModel):
    table: Literal["facts"] = "facts"
    id: UUID
    space_id: UUID
    scope: Scope
    subject: EntityRef
    predicate: str
    value: Any = None
    value_text: str
    confidence: float
    importance: float
    status: FactStatus
    conflict: bool = False
    supersedes_id: UUID | None = None
    superseded_by: UUID | None = None
    valid_from: datetime | None = None
    valid_to: datetime | None = None
    first_seen_at: datetime
    last_confirmed_at: datetime
    confirmations: int = 1
    evidence_observation_ids: list[UUID] = Field(default_factory=list)
    learned_on_client_ids: list[UUID] = Field(default_factory=list)
    access_count: int = 0
    last_accessed_at: datetime | None = None
    pinned: bool = False
    sensitivity: Sensitivity = "normal"
    written_by_client_id: UUID | None = None
    extracted_by: dict[str, Any] | None = None


class ProcedureFull(BaseModel):
    table: Literal["procedures"] = "procedures"
    id: UUID
    space_id: UUID
    scope: Scope
    name: str
    task_class: str
    trigger_text: str
    body_md: str
    version: int = 1
    status: Literal["active", "retired"] = "active"
    supersedes_id: UUID | None = None
    success_count: int = 0
    failure_count: int = 0
    last_used_at: datetime | None = None
    source_observation_ids: list[UUID] = Field(default_factory=list)
    token_estimate: int = 0
    created_at: datetime


MemoryItem = Annotated[ObservationFull | SummaryFull | FactFull | ProcedureFull, Field(discriminator="table")]


class GetRequest(BaseModel):
    ids: list[UUID] = Field(min_length=1, max_length=MAX_GET_IDS)


class GetResponse(BaseModel):
    items: list[MemoryItem]
    missing: list[UUID] = Field(default_factory=list)  # unknown or not visible to the caller


# ---------- explain (12.5) ----------


class SessionInfo(BaseModel):
    id: UUID
    harness: str
    project_key: str | None = None
    device_id: str | None = None
    hostname: str | None = None
    client_kind: str | None = None
    started_at: datetime


class Provenance(BaseModel):
    """Where an observation/summary came from: its session, raw event seqs, and the extractor."""

    session: SessionInfo | None = None
    event_ids: list[UUID] = Field(default_factory=list)
    event_seqs: list[int] = Field(default_factory=list)
    extracted_by: dict[str, Any] = Field(default_factory=dict)


class EvidenceObservation(BaseModel):
    id: UUID
    type: str
    title: str
    created_at: datetime
    provenance: Provenance


class AuditEntry(BaseModel):
    id: UUID
    actor: str
    action: str
    before: dict[str, Any] | None = None
    after: dict[str, Any] | None = None
    reason: str | None = None
    model: str | None = None
    prompt_version: str | None = None
    run_id: UUID | None = None
    created_at: datetime


class ExplainResponse(BaseModel):
    """Facts/procedures: ``evidence`` lists the observations they rest on, each with provenance.
    Observations/summaries: ``provenance`` is filled directly and ``evidence`` is empty."""

    id: UUID
    table: MemoryTable
    item: MemoryItem
    provenance: Provenance | None = None
    evidence: list[EvidenceObservation] = Field(default_factory=list)
    audit: list[AuditEntry] = Field(default_factory=list)


# ---------- explicit writes ----------


class FactSubject(BaseModel):
    kind: str = "user"
    name: str


class _RememberBase(BaseModel):
    scope: Scope | None = None  # default: project scope of the caller's current project, else global
    space_id: UUID | None = None  # default: the caller's default space
    sensitivity: Sensitivity = "normal"


class RememberFact(_RememberBase):
    """Lands ``active`` with confidence 0.95 and ``written_by_client_id`` set (spec 6.4)."""

    kind: Literal["fact"]
    subject: FactSubject
    predicate: str = Field(pattern=r"^[a-z0-9_]+(\.[a-z0-9_]+)*$")
    value: Any = None
    value_text: str = Field(min_length=1, max_length=300)
    pinned: bool = False


class RememberObservation(_RememberBase):
    """``extracted_by = {"runner_kind": "explicit"}``. MCP ``memory_remember(kind="note")`` maps here."""

    kind: Literal["observation"]
    type: ObservationType = "discovery"
    title: str = Field(min_length=1, max_length=120)
    subtitle: str | None = Field(None, max_length=200)
    narrative: str | None = Field(None, max_length=2000)
    facts: list[str] = Field(default_factory=list, max_length=10)
    concepts: list[Concept] = Field(default_factory=list, max_length=5)
    files: list[str] = Field(default_factory=list)
    importance: float = Field(0.7, ge=0, le=1)


class RememberProcedure(_RememberBase):
    kind: Literal["procedure"]
    name: str = Field(min_length=1)
    task_class: str = Field(min_length=1)
    trigger_text: str = Field(min_length=1)
    body_md: str = Field(min_length=1)


RememberRequest = Annotated[RememberFact | RememberObservation | RememberProcedure, Field(discriminator="kind")]


class RememberResponse(BaseModel):
    id: UUID
    table: MemoryTable
    status: str  # 'active' for facts/procedures, 'created' for observations
    rendered: str  # how it will be shown in context (index row or citation line)


class ForgetResponse(BaseModel):
    """``DELETE /v1/memories/{id}``: soft unless ``?hard=true`` (admin)."""

    id: UUID
    table: MemoryTable
    action: Literal["retracted", "archived", "retired", "deleted"]
