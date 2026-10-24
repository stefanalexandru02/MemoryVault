"""Spec 6.5: work state (append-only rows, folded on read; last write per key wins)."""

from __future__ import annotations

from typing import Any
from uuid import UUID

from pydantic import BaseModel, ConfigDict, Field

from memsvc.schemas.common import Scope
from memsvc.schemas.recall import WorkStateList


class WorkStateQuery(BaseModel):
    """``GET /v1/work-state?scope=kind:key&list=name`` (both optional)."""

    model_config = ConfigDict(populate_by_name=True)

    scope: Scope | None = None
    list_name: str | None = Field(None, alias="list")
    space_ids: list[UUID] | None = None


class WorkStateWrite(BaseModel):
    scope: Scope | None = None  # default: the caller's current project scope, else global
    space_id: UUID | None = None
    list_name: str = Field(min_length=1, max_length=120)
    fields: dict[str, Any] = Field(min_length=1)


class WorkStateResponse(BaseModel):
    lists: list[WorkStateList]


__all__ = ["WorkStateList", "WorkStateQuery", "WorkStateResponse", "WorkStateWrite"]
