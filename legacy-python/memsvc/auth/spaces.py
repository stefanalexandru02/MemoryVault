"""Readable/writable space resolution (spec 5.3, architecture 13).

Explicit ``space_id``s in a request are validated against the caller's memberships *before* any
query: a space the user is not a member of yields 403. Without explicit ids, reads default to the
user's ``default_space_id`` plus every shared space they belong to; writes default to
``default_space_id``. ``memberships`` is RLS-limited to the caller's own rows for ``memsvc_app``, so
call these on a ``user_tx`` connection for the same user (or a ``system_tx`` connection).
"""

from __future__ import annotations

from collections.abc import Sequence
from uuid import UUID

import asyncpg

from memsvc.auth.identity import Identity
from memsvc.errors import ApiError


async def readable_space_ids(conn: asyncpg.Connection, user_id: UUID) -> list[UUID]:
    rows = await conn.fetch("SELECT space_id FROM memberships WHERE user_id = $1 ORDER BY space_id", user_id)
    return [r["space_id"] for r in rows]


async def writable_space_ids(conn: asyncpg.Connection, user_id: UUID) -> list[UUID]:
    rows = await conn.fetch(
        "SELECT space_id FROM memberships WHERE user_id = $1 AND role IN ('owner','writer') ORDER BY space_id",
        user_id,
    )
    return [r["space_id"] for r in rows]


async def resolve_read_spaces(
    conn: asyncpg.Connection, identity: Identity, requested: Sequence[UUID] | None = None
) -> list[UUID]:
    """Spaces a read covers. Raises 403 if any requested id is not readable by the caller."""
    if requested:
        readable = set(await readable_space_ids(conn, identity.user_id))
        foreign = [s for s in requested if s not in readable]
        if foreign:
            raise ApiError(403, "space_forbidden", f"not a member of space {foreign[0]}")
        return list(dict.fromkeys(requested))
    rows = await conn.fetch(
        """SELECT m.space_id FROM memberships m JOIN spaces s ON s.id = m.space_id
            WHERE m.user_id = $1 AND (s.id = $2 OR s.kind = 'shared')
            ORDER BY (s.id = $2) DESC, s.id""",
        identity.user_id,
        identity.default_space_id,
    )
    return [r["space_id"] for r in rows]


async def resolve_write_space(conn: asyncpg.Connection, identity: Identity, requested: UUID | None = None) -> UUID:
    """The space a write lands in. Raises 403 unless the caller is owner/writer of it."""
    target = requested or identity.default_space_id
    if target is None:
        raise ApiError(403, "space_forbidden", "no space given and the user has no default space")
    if target not in set(await writable_space_ids(conn, identity.user_id)):
        raise ApiError(403, "space_forbidden", f"no write access to space {target}")
    return target
