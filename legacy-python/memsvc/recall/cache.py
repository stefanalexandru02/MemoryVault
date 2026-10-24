"""Context cache (spec 11.6). Owner: recall.

Rows live in ``context_cache`` (migration 0009). Invalidation uses the SECURITY DEFINER SQL function
``memsvc_invalidate_context(space, kind, key)`` so the API's listener (no user context) can delete
every user's cached block for a changed scope. Writers signal with ``memsvc.invalidate.notify_invalidate``.
"""

from __future__ import annotations

import asyncio

import asyncpg

from memsvc.db import Database


async def invalidate(conn: asyncpg.Connection, space_id: str, scope_kind: str | None, scope_key: str | None) -> int:
    """Delete cached blocks that include ``(space, scope)``; ``scope_kind=None`` = whole space."""
    return await conn.fetchval("SELECT memsvc_invalidate_context($1::uuid, $2, $3)", space_id, scope_kind, scope_key)


async def start_listener(db: Database) -> asyncio.Task[None] | None:
    """Start the API-side ``LISTEN memory_invalidate`` loop (called from app startup). Returns the task."""
    raise NotImplementedError


async def rerender(db: Database, cache_keys: list[str]) -> int:
    """Worker hook: re-render the given cache keys after the 2 s debounce. Returns rows re-rendered."""
    raise NotImplementedError
