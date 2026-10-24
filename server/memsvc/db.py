"""asyncpg pools, transaction helpers and database capability detection.

Conventions (see CONTRACTS.md):

- ``json``/``jsonb`` columns round-trip as Python objects (dict/list/...). Pass dicts, not strings.
  UUIDs, datetimes and pydantic models inside them are serialised automatically.
- ``vector`` columns round-trip as ``list[float]``; any sequence of numbers (or a numpy array) is
  accepted as a parameter.
- API/MCP code runs inside :func:`user_tx` so RLS applies (spec 4.7, 5.3). Broker, planner and worker
  code runs inside :func:`system_tx` (role ``memsvc_system``, BYPASSRLS).
- In tests both pools connect as the ``postgres`` superuser; the helpers then ``SET LOCAL ROLE`` to
  ``memsvc_app`` / ``memsvc_system`` so grants and policies are exercised exactly as in production.
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator, Sequence
from contextlib import asynccontextmanager
from dataclasses import dataclass
from datetime import date, datetime
from decimal import Decimal
from typing import Any
from uuid import UUID

import asyncpg
from pydantic import BaseModel

from memsvc.config import Settings

APP_ROLE = "memsvc_app"
SYSTEM_ROLE = "memsvc_system"


@dataclass(frozen=True)
class DbCaps:
    """What the connected database supports; detected once per connection at pool init."""

    superuser: bool
    current_user: str
    pg_search: bool  # ParadeDB BM25 available -> use `@@@` / paradedb.score; else tsvector fallback (11.2)
    pgvector_version: tuple[int, ...] | None

    @property
    def hnsw_iterative_scan(self) -> bool:
        """``hnsw.iterative_scan`` GUC exists only in pgvector >= 0.8."""
        return self.pgvector_version is not None and self.pgvector_version >= (0, 8)


class MemConnection(asyncpg.Connection):
    """asyncpg connection carrying its :class:`DbCaps` (read with :func:`caps`)."""

    memsvc_caps: DbCaps | None = None


def _json_default(o: Any) -> Any:
    if isinstance(o, UUID):
        return str(o)
    if isinstance(o, datetime | date):
        return o.isoformat()
    if isinstance(o, Decimal):
        return float(o)
    if isinstance(o, BaseModel):
        return o.model_dump(mode="json")
    if isinstance(o, set | frozenset | tuple):
        return list(o)
    raise TypeError(f"not JSON serializable: {type(o).__name__}")


def json_dumps(value: Any) -> str:
    return json.dumps(value, default=_json_default, ensure_ascii=False, separators=(",", ":"))


def _encode_vector(value: Sequence[float] | Any) -> str:
    return "[" + ",".join(format(float(x), ".9g") for x in value) + "]"


def _decode_vector(text: str) -> list[float]:
    inner = text.strip()[1:-1]
    return [float(x) for x in inner.split(",")] if inner else []


def _parse_version(v: str | None) -> tuple[int, ...] | None:
    if not v:
        return None
    parts: list[int] = []
    for p in v.split("."):
        digits = "".join(ch for ch in p if ch.isdigit())
        parts.append(int(digits or 0))
    return tuple(parts)


async def _init_connection(conn: asyncpg.Connection) -> None:
    for typ in ("json", "jsonb"):
        await conn.set_type_codec(typ, encoder=json_dumps, decoder=json.loads, schema="pg_catalog")
    row = await conn.fetchrow(
        """
        SELECT r.rolsuper, current_user AS cu,
               EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_search') AS pg_search,
               (SELECT extversion FROM pg_extension WHERE extname = 'vector') AS vector_version,
               (SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
                 WHERE e.extname = 'vector') AS vector_schema
        FROM pg_roles r WHERE r.rolname = current_user
        """
    )
    assert row is not None
    if row["vector_schema"]:
        await conn.set_type_codec(
            "vector",
            encoder=_encode_vector,
            decoder=_decode_vector,
            schema=row["vector_schema"],
            format="text",
        )
    conn.memsvc_caps = DbCaps(  # type: ignore[attr-defined]
        superuser=bool(row["rolsuper"]),
        current_user=row["cu"],
        pg_search=bool(row["pg_search"]),
        pgvector_version=_parse_version(row["vector_version"]),
    )


async def create_pool(dsn: str, *, min_size: int = 1, max_size: int = 10) -> asyncpg.Pool:
    return await asyncpg.create_pool(
        dsn,
        min_size=min_size,
        max_size=max_size,
        init=_init_connection,
        connection_class=MemConnection,
    )


@dataclass
class Database:
    """The process's pools. ``system`` is None when DATABASE_SYSTEM_URL is unset."""

    app: asyncpg.Pool
    system: asyncpg.Pool | None

    async def close(self) -> None:
        await self.app.close()
        if self.system is not None and self.system is not self.app:
            await self.system.close()


async def connect(settings: Settings) -> Database:
    """Open the app pool and, if configured, the system pool (shared when both URLs are equal)."""
    app = await create_pool(settings.database_url, min_size=settings.db_pool_min, max_size=settings.db_pool_max)
    system: asyncpg.Pool | None = None
    if settings.database_system_url:
        if settings.database_system_url == settings.database_url:
            system = app
        else:
            system = await create_pool(
                settings.database_system_url, min_size=settings.db_pool_min, max_size=settings.db_pool_max
            )
    return Database(app=app, system=system)


def caps(conn: asyncpg.Connection) -> DbCaps:
    """Capabilities of the database behind ``conn`` (works on pool connection proxies)."""
    c = getattr(conn, "memsvc_caps", None)
    if c is None:
        raise RuntimeError("connection was not created by memsvc.db.create_pool")
    return c


@asynccontextmanager
async def user_tx(
    pool: asyncpg.Pool, user_id: UUID | str, client_id: UUID | str | None
) -> AsyncIterator[asyncpg.Connection]:
    """Transaction as ``memsvc_app`` with ``app.user_id`` / ``app.client_id`` set (spec 5.3).

    RLS policies read ``app_user_id()``; nothing outside the user's spaces is visible.
    """
    async with pool.acquire() as conn:
        async with conn.transaction():
            if caps(conn).current_user != APP_ROLE:
                await conn.execute(f"SET LOCAL ROLE {APP_ROLE}")
            await conn.execute(
                "SELECT set_config('app.user_id', $1, true), set_config('app.client_id', $2, true)",
                str(user_id),
                "" if client_id is None else str(client_id),
            )
            yield conn


@asynccontextmanager
async def system_tx(pool: asyncpg.Pool | None) -> AsyncIterator[asyncpg.Connection]:
    """Transaction as ``memsvc_system`` (RLS bypass) for the broker, worker and planner."""
    if pool is None:
        raise RuntimeError("no system pool: set DATABASE_SYSTEM_URL")
    async with pool.acquire() as conn:
        async with conn.transaction():
            if caps(conn).superuser:
                await conn.execute(f"SET LOCAL ROLE {SYSTEM_ROLE}")
            yield conn


async def tune_vector_search(conn: asyncpg.Connection, ef_search: int = 80) -> None:
    """Per-transaction HNSW settings for a recall (spec 4.5). Call inside a transaction."""
    await conn.execute(f"SET LOCAL hnsw.ef_search = {int(ef_search)}")
    if caps(conn).hnsw_iterative_scan:
        await conn.execute("SET LOCAL hnsw.iterative_scan = relaxed_order")
