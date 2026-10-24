"""Spec 6.1: sessions."""

from __future__ import annotations

from datetime import datetime
from typing import Any
from uuid import UUID

from pydantic import BaseModel, Field


class SessionCreate(BaseModel):
    harness: str = "claude-code"
    external_session_id: str = Field(min_length=1, max_length=256)
    project_key: str | None = None
    cwd: str | None = None
    space_id: UUID | None = None
    metadata: dict[str, Any] = Field(default_factory=dict)


class WindowPolicy(BaseModel):
    tool_uses: int = 8
    idle_seconds: int = 90


class SessionCreated(BaseModel):
    """201 on create, 200 with the existing id on a repeat of ``(client, external_session_id)``."""

    session_id: UUID
    space_id: UUID
    window_policy: WindowPolicy = Field(default_factory=WindowPolicy)


class SessionEnd(BaseModel):
    reason: str | None = None


class SessionEnded(BaseModel):
    session_id: UUID
    ended_at: datetime
    window_closed: bool  # the open window was closed (an extract packet was created if it had content)
    packets_created: list[str] = Field(default_factory=list)  # packet kinds created by this call
