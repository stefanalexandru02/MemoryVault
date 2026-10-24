"""Spec 6.6 / 9: work packets as seen by runners (lease, result, fail, heartbeat) and by the server."""

from __future__ import annotations

from datetime import datetime
from typing import Any, Literal
from uuid import UUID

from pydantic import BaseModel, ConfigDict, Field

from memsvc.schemas.common import OutputSchemaName, PacketKind, PacketStatus

DEFAULT_LEASE_KINDS: list[PacketKind] = ["extract", "summarize_prompt", "summarize_session"]


class LeaseRequest(BaseModel):
    session_id: UUID | None = None  # restrict to one session's packets; None = any packet the caller may run
    kinds: list[PacketKind] = Field(default_factory=lambda: list(DEFAULT_LEASE_KINDS), min_length=1)
    max: int = Field(1, ge=1, le=50)
    lease_seconds: int = Field(600, ge=30, le=3600)


class PacketPrompt(BaseModel):
    """Rendered prompt text so runners need no prompt files. ``user`` is ``user_template`` rendered
    with the packet's inputs (what mem-observe passes to ``claude -p``)."""

    system: str
    user_template: str
    user: str


class LeasedPacket(BaseModel):
    id: UUID
    kind: PacketKind
    prompt_ref: str
    model_hint: str
    inputs: dict[str, Any]
    output_schema: OutputSchemaName
    lease_until: datetime
    attempts: int = 0
    prompt: PacketPrompt


class LeaseResponse(BaseModel):
    packets: list[LeasedPacket]


class ResultMeta(BaseModel):
    model_config = ConfigDict(extra="allow")

    model: str | None = None
    duration_ms: int | None = None
    cost_usd: float | None = None
    runner_kind: str | None = None  # 'claude-p'
    runner: str | None = None  # device id


class ResultRequest(BaseModel):
    """``output`` is the model's JSON object, or its raw text (a fenced block is stripped, spec 9.4)."""

    output: dict[str, Any] | str
    meta: ResultMeta = Field(default_factory=ResultMeta)


class ResultResponse(BaseModel):
    """200. A duplicate post returns the first application's counts with ``duplicate = true`` (9.3)."""

    applied: dict[str, int]
    duplicate: bool = False


class FailRequest(BaseModel):
    error: Literal["quota_exhausted", "cli_missing", "timeout", "other"]
    detail: str | None = None


class FailResponse(BaseModel):
    id: UUID
    status: PacketStatus
    attempts: int
    available_after: datetime | None = None


class HeartbeatResponse(BaseModel):
    id: UUID
    lease_until: datetime


class PacketRecord(BaseModel):
    """A ``work_packets`` row (``PacketRecord.model_validate(dict(row))``). Passed to appliers and
    returned by ``GET /v1/admin/packets``."""

    id: UUID
    space_id: UUID
    kind: PacketKind
    session_id: UUID | None = None
    preferred_client_id: UUID | None = None
    prompt_ref: str
    model_hint: str
    inputs: dict[str, Any]
    output_schema: OutputSchemaName
    status: PacketStatus
    attempts: int = 0
    max_attempts: int = 3
    leased_by: UUID | None = None
    lease_until: datetime | None = None
    available_after: datetime
    result: dict[str, Any] | None = None
    result_meta: dict[str, Any] | None = None
    last_error: str | None = None
    idempotency_key: str
    created_at: datetime
    done_at: datetime | None = None
