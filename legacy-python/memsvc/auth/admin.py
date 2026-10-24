"""Tenancy bootstrap helpers shared by the admin API, the admin CLI and tests.

Run them on a system connection (``db.system_tx``); users/spaces/memberships/clients/api_tokens have
no RLS but writes to them are admin-only operations.
"""

from __future__ import annotations

from dataclasses import dataclass
from uuid import UUID

import asyncpg

from memsvc.ids import uuid7


@dataclass(frozen=True)
class CreatedUser:
    user_id: UUID
    space_id: UUID  # the private default space


async def create_space(conn: asyncpg.Connection, *, name: str, kind: str, owner_id: UUID) -> UUID:
    """Create a space and make ``owner_id`` its owner member."""
    space_id = uuid7()
    await conn.execute(
        "INSERT INTO spaces (id, name, kind, owner_id) VALUES ($1, $2, $3, $4)", space_id, name, kind, owner_id
    )
    await add_membership(conn, space_id=space_id, user_id=owner_id, role="owner")
    return space_id


async def add_membership(conn: asyncpg.Connection, *, space_id: UUID, user_id: UUID, role: str) -> None:
    await conn.execute(
        """INSERT INTO memberships (space_id, user_id, role) VALUES ($1, $2, $3)
           ON CONFLICT (space_id, user_id) DO UPDATE SET role = EXCLUDED.role""",
        space_id,
        user_id,
        role,
    )


async def create_user(conn: asyncpg.Connection, *, handle: str, display_name: str | None = None) -> CreatedUser:
    """Create a user with one private space set as ``default_space_id`` (spec D4)."""
    user_id = uuid7()
    await conn.execute(
        "INSERT INTO users (id, handle, display_name) VALUES ($1, $2, $3)", user_id, handle, display_name or handle
    )
    space_id = await create_space(conn, name=f"{handle} (private)", kind="private", owner_id=user_id)
    await conn.execute("UPDATE users SET default_space_id = $2 WHERE id = $1", user_id, space_id)
    return CreatedUser(user_id, space_id)


async def ensure_client(
    conn: asyncpg.Connection,
    *,
    user_id: UUID,
    device_id: str,
    kind: str = "claude-code",
    hostname: str | None = None,
    platform: str | None = None,
) -> UUID:
    """Return the client id for ``(user, device_id)``, creating it if missing."""
    row = await conn.fetchrow(
        """INSERT INTO clients (id, user_id, kind, device_id, hostname, platform) VALUES ($1, $2, $3, $4, $5, $6)
           ON CONFLICT (user_id, device_id) DO UPDATE
             SET hostname = coalesce(EXCLUDED.hostname, clients.hostname),
                 platform = coalesce(EXCLUDED.platform, clients.platform)
           RETURNING id""",
        uuid7(),
        user_id,
        kind,
        device_id,
        hostname,
        platform,
    )
    assert row is not None
    return row["id"]
