"""Spec 9.2: packet ``inputs`` for extract / summarize packets (immutable at creation).

Prompt templates (``server/prompts/*.yaml``, Jinja2) render these fields. Consolidation packet
inputs are defined by the consolidation module (spec 12) and stored as plain dicts.
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Literal
from uuid import UUID

from pydantic import BaseModel, Field


class PacketSessionInfo(BaseModel):
    harness: str
    project_key: str | None = None
    device: str | None = None  # clients.device_id
    started_at: datetime


class PacketEvent(BaseModel):
    seq: int
    kind: str
    at: datetime
    tool: str | None = None  # tool_use / tool_failure
    input: Any = None  # tool_input
    output: Any = None  # tool_response, or the error for tool_failure
    text: str | None = None  # user_prompt text, assistant/subagent last message, system/note text


class RecentTitle(BaseModel):
    id: UUID
    type: str
    title: str


class ExtractInputs(BaseModel):
    mode: Literal["code"] = "code"
    session: PacketSessionInfo
    window_no: int
    events: list[PacketEvent]
    recent_titles: list[RecentTitle] = Field(default_factory=list)  # last 20 observations in this session
    profile_md: str = ""  # project-scope profile block, <= 400 tokens
    known_entities: list[str] = Field(default_factory=list)


class SummarizePromptInputs(BaseModel):
    mode: Literal["code"] = "code"
    session: PacketSessionInfo
    prompt_no: int
    prompt: str | None = None  # the user prompt text
    observation_titles: list[RecentTitle] = Field(default_factory=list)
    last_assistant_message: str | None = None


class PromptSummaryIn(BaseModel):
    prompt_no: int | None = None
    request: str | None = None
    investigated: str | None = None
    learned: str | None = None
    completed: str | None = None
    next_steps: str | None = None
    notes: str | None = None


class SummarizeSessionInputs(BaseModel):
    mode: Literal["code"] = "code"
    session: PacketSessionInfo
    prompt_summaries: list[PromptSummaryIn]
