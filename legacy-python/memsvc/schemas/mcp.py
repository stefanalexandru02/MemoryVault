"""Spec 7: MCP tool arguments. Results are markdown text (budgeted < 8k tokens, 40k chars)."""

from __future__ import annotations

from typing import Any, Literal

from pydantic import BaseModel, Field

from memsvc.schemas.memories import MAX_GET_IDS

MAX_RESULT_CHARS = 40_000  # _meta["anthropic/maxResultSizeChars"]


class MemorySearchArgs(BaseModel):
    query: str
    types: list[str] | None = None
    scope: Literal["project", "global", "all"] = "all"
    since: str | None = None
    files: list[str] | None = None
    limit: int = Field(20, ge=1, le=30)


class MemoryTimelineArgs(BaseModel):
    anchor_id: str
    before: int = Field(5, ge=0, le=10)
    after: int = Field(5, ge=0, le=10)


class MemoryGetArgs(BaseModel):
    ids: list[str] = Field(min_length=1, max_length=MAX_GET_IDS)


class MemoryExplainArgs(BaseModel):
    id: str


class MemoryRememberArgs(BaseModel):
    kind: Literal["fact", "note", "procedure"]
    content: str = Field(min_length=1)
    scope: Literal["project", "global"] | None = None
    subject: str | None = None
    predicate: str | None = None


class MemoryForgetArgs(BaseModel):
    id: str
    reason: str | None = None


class WorkStateWriteArgs(BaseModel):
    list: str = Field(min_length=1)
    fields: dict[str, Any] = Field(min_length=1)
