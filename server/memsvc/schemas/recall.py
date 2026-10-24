"""Spec 6.3: push-mode recall, context block, feedback. Slot item shapes are this module's contract."""

from __future__ import annotations

from datetime import datetime
from typing import Any, Literal
from uuid import UUID

from pydantic import BaseModel, Field

from memsvc.schemas.common import Scope, Sensitivity, SummaryGrain


class TaskState(BaseModel):
    intent: str | None = None  # matched against procedures.task_class (+0.3, spec 11.2)
    entities: list[str] = Field(default_factory=list)  # appended to the keyword query with lower weight


class Budgets(BaseModel):
    """Token budget per slot (spec 11.5). 0 disables a slot."""

    profile: int = Field(400, ge=0)
    facts: int = Field(300, ge=0)
    procedures: int = Field(400, ge=0)
    observations: int = Field(600, ge=0)
    summaries: int = Field(300, ge=0)
    work_state: int = Field(200, ge=0)


class RecallRequest(BaseModel):
    space_ids: list[UUID] | None = None  # default: readable spaces (default + shared), spec 5.3
    scopes: list[Scope] | None = None  # default: global + the caller's device; recall decides
    query: str | None = None
    task_state: TaskState | None = None
    budgets: Budgets = Field(default_factory=Budgets)
    expand_top_observations: int = Field(3, ge=0, le=20)
    rerank: bool = True
    exclude_sensitivity: list[Sensitivity] = Field(default_factory=lambda: ["secret"])
    format: Literal["json", "markdown", "both"] = "both"


class SlotItem(BaseModel):
    """Common fields of every ranked slot item."""

    id: UUID
    space_id: UUID
    scope: Scope
    score: float = 0.0
    token_estimate: int = 0
    citation: str | None = None  # e.g. "[fact#a91c2e · 0.83 · laptop+desktop · 2026-10-02]"


class ProfileSlot(BaseModel):
    body_md: str
    token_estimate: int
    scopes: list[Scope] = Field(default_factory=list)


class FactItem(SlotItem):
    subject: str  # entity canonical_name
    subject_kind: str | None = None
    predicate: str
    value: Any = None
    value_text: str
    confidence: float
    importance: float = 0.5
    pinned: bool = False
    learned_on: list[str] = Field(default_factory=list)  # device ids
    first_seen_at: datetime | None = None
    last_confirmed_at: datetime | None = None


class ProcedureItem(SlotItem):
    name: str
    task_class: str
    trigger_text: str
    body_md: str
    version: int = 1
    success_count: int = 0
    last_used_at: datetime | None = None


class ObservationItem(SlotItem):
    type: str
    title: str
    subtitle: str | None = None
    narrative: str | None = None  # present only when expanded
    facts: list[str] = Field(default_factory=list)  # present only when expanded
    files: list[str] = Field(default_factory=list)
    importance: float = 0.5
    created_at: datetime
    session_id: UUID | None = None
    device_id: str | None = None
    expanded: bool = False


class SummaryItem(SlotItem):
    grain: SummaryGrain
    request: str | None = None
    learned: str | None = None
    completed: str | None = None
    next_steps: str | None = None
    created_at: datetime
    session_id: UUID | None = None
    device_id: str | None = None


class WorkStateList(BaseModel):
    """A folded work-state list (last write per key wins), spec 6.5."""

    scope: Scope
    list_name: str
    fields: dict[str, Any]
    updated_at: datetime | None = None
    token_estimate: int = 0


class RecallSlots(BaseModel):
    profile: ProfileSlot | None = None
    facts: list[FactItem] = Field(default_factory=list)
    procedures: list[ProcedureItem] = Field(default_factory=list)
    observations: list[ObservationItem] = Field(default_factory=list)
    summaries: list[SummaryItem] = Field(default_factory=list)
    work_state: list[WorkStateList] = Field(default_factory=list)


class TokenUsage(BaseModel):
    total: int
    by_slot: dict[str, int] = Field(default_factory=dict)


class RecallResponse(BaseModel):
    retrieval_id: UUID
    slots: RecallSlots
    rendered_markdown: str | None = None  # None when format == "json"
    tokens: TokenUsage
    latency_ms: int
    reranked: bool


class ContextQuery(BaseModel):
    """``GET /v1/context`` query params; the response is ``text/markdown`` with an ETag."""

    project_key: str | None = None
    device_id: str | None = None
    source: Literal["startup", "resume", "clear", "compact"] = "startup"


class FeedbackRequest(BaseModel):
    used_ids: list[UUID] = Field(default_factory=list)
    helpful: bool | None = None
    notes: str | None = None


class FeedbackResponse(BaseModel):
    retrieval_id: UUID
    recorded: bool = True


__all__ = [
    "Budgets",
    "ContextQuery",
    "FactItem",
    "FeedbackRequest",
    "FeedbackResponse",
    "ObservationItem",
    "ProcedureItem",
    "ProfileSlot",
    "RecallRequest",
    "RecallResponse",
    "RecallSlots",
    "SlotItem",
    "SummaryItem",
    "TaskState",
    "TokenUsage",
    "WorkStateList",
]
