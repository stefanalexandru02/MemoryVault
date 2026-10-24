"""Spec 6.2: event batches."""

from __future__ import annotations

from datetime import datetime
from typing import Any

from pydantic import BaseModel, Field

from memsvc.schemas.common import EventKind

MAX_EVENTS_PER_BATCH = 200
MAX_BODY_BYTES = 4 * 1024 * 1024
FIELD_HEAD_CHARS = 6144
FIELD_TAIL_CHARS = 2048
PAYLOAD_CAP_BYTES = 64 * 1024


class EventIn(BaseModel):
    seq: int = Field(ge=0)
    kind: EventKind
    occurred_at: datetime
    idempotency_key: str = Field(min_length=1, max_length=256)
    payload: dict[str, Any] = Field(default_factory=dict)


class EventsBatch(BaseModel):
    events: list[EventIn] = Field(min_length=1, max_length=MAX_EVENTS_PER_BATCH)


class EventsAccepted(BaseModel):
    """202 response."""

    accepted: int
    duplicates: int
    window_no: int
    window_closed: bool
