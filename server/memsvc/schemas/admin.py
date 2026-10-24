"""Spec 6.7: admin API (admin scope)."""

from __future__ import annotations

from datetime import datetime
from typing import Any, Literal
from uuid import UUID

from pydantic import BaseModel, Field

from memsvc.schemas.common import ClientKind, MembershipRole, PacketKind, SpaceKind


class UserCreate(BaseModel):
    handle: str = Field(min_length=1, max_length=64, pattern=r"^[a-z0-9][a-z0-9_.-]*$")
    display_name: str | None = None


class UserOut(BaseModel):
    id: UUID
    handle: str
    display_name: str
    default_space_id: UUID | None = None
    created_at: datetime
    disabled_at: datetime | None = None


class SpaceCreate(BaseModel):
    name: str = Field(min_length=1)
    kind: SpaceKind = "shared"
    owner_id: UUID


class SpaceOut(BaseModel):
    id: UUID
    name: str
    kind: SpaceKind
    owner_id: UUID
    created_at: datetime


class MembershipCreate(BaseModel):
    space_id: UUID
    user_id: UUID
    role: MembershipRole


class MembershipOut(MembershipCreate):
    pass


class TokenCreate(BaseModel):
    """Mint a token; the client ``(user, device_id)`` is created when missing."""

    user_id: UUID | None = None
    handle: str | None = None  # alternative to user_id
    device_id: str = Field(min_length=1)
    client_kind: ClientKind | None = None  # default from token kind: device->claude-code, runner->runner, admin->cli
    hostname: str | None = None
    platform: str | None = None
    kind: Literal["device", "runner", "admin"] = "device"
    expires_days: int | None = Field(None, ge=1, le=3650)


class TokenMinted(BaseModel):
    """The raw ``token`` is returned exactly once."""

    token: str
    token_id: UUID
    prefix: str
    user_id: UUID
    client_id: UUID
    kind: str
    scopes: list[str]
    expires_at: datetime | None = None


class TokenOut(BaseModel):
    id: UUID
    user_id: UUID
    client_id: UUID
    kind: str
    prefix: str
    scopes: list[str]
    created_at: datetime
    expires_at: datetime | None = None
    revoked_at: datetime | None = None
    last_used_at: datetime | None = None


class ConsolidateRequest(BaseModel):
    space_id: UUID | None = None  # None = every space


class ConsolidateResponse(BaseModel):
    run_id: UUID
    packets_created: dict[str, int] = Field(default_factory=dict)  # by packet kind
    actions: dict[str, int] = Field(default_factory=dict)  # code-only actions: auto-activate, archive, ...


class RevertResponse(BaseModel):
    run_id: UUID
    reverted: int
    packets_cancelled: int


class JobOut(BaseModel):
    id: UUID
    type: str
    payload: dict[str, Any]
    status: str
    attempts: int
    run_after: datetime
    locked_by: str | None = None
    locked_until: datetime | None = None
    last_error: str | None = None
    created_at: datetime


class PacketRetryRequest(BaseModel):
    reset_attempts: bool = True


class ReindexRequest(BaseModel):
    model: str | None = None  # default: settings.embed_model
    tables: list[Literal["observations", "summaries", "facts", "procedures", "entities"]] | None = None
    space_id: UUID | None = None


class ReindexResponse(BaseModel):
    jobs_enqueued: int


class PacketCounts(BaseModel):
    by_kind: dict[PacketKind, int] = Field(default_factory=dict)
