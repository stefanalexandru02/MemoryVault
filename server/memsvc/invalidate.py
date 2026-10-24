"""Context-cache invalidation signal (spec 11.6).

Every write path that changes what a SessionStart block or recall would show calls
:func:`notify_invalidate` inside its transaction. Postgres delivers the NOTIFY on commit to every
listener on channel ``memory_invalidate`` with payload ``<space_id>:<scope_kind>:<scope_key>``.
The API's listener (``memsvc.recall.cache``) deletes matching ``context_cache`` rows; the worker
re-renders them after a debounce.
"""

from __future__ import annotations

from uuid import UUID

import asyncpg

CHANNEL = "memory_invalidate"


def format_payload(space_id: UUID | str, scope_kind: str, scope_key: str) -> str:
    return f"{space_id}:{scope_kind}:{scope_key}"


def parse_payload(payload: str) -> tuple[UUID, str, str]:
    """Inverse of :func:`format_payload`. ``scope_key`` may itself contain ':'."""
    space, kind, key = payload.split(":", 2)
    return UUID(space), kind, key


async def notify_invalidate(conn: asyncpg.Connection, space_id: UUID | str, scope_kind: str, scope_key: str) -> None:
    await conn.execute("SELECT pg_notify($1, $2)", CHANNEL, format_payload(space_id, scope_kind, scope_key))
