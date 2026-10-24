"""The authenticated caller, as resolved from a bearer token."""

from __future__ import annotations

from dataclasses import dataclass
from uuid import UUID


@dataclass(frozen=True)
class Identity:
    user_id: UUID
    client_id: UUID
    scopes: frozenset[str]
    default_space_id: UUID | None
    token_id: UUID | None = None
    token_kind: str | None = None
    device_id: str | None = None  # clients.device_id of the token's client
    client_kind: str | None = None  # clients.kind

    def has(self, scope: str) -> bool:
        return scope in self.scopes

    @property
    def actor(self) -> str:
        """memory_audit.actor for writes made by this caller."""
        return f"client:{self.client_id}"
