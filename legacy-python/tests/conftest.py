"""Shared test fixtures (owned by the foundation).

Each pytest process gets its own database ``memsvc_test_<pid>`` on the test cluster
(``MEMSVC_TEST_ADMIN_DSN``, default ``postgresql://postgres@127.0.0.1:55432/postgres``), migrated
from ``server/migrations``, dropped at the end. Both pools connect as the superuser;
``memsvc.db.user_tx`` / ``system_tx`` switch to ``memsvc_app`` / ``memsvc_system`` per transaction.

Fixtures:
  scratch_database   async context manager factory for an extra empty database
  settings           Settings built from the test environment (FAKE_TEI=1, high rate limits)
  db                 memsvc.db.Database for the test database (session-scoped)
  make_identity      async factory -> Seeded (user, private space, client, minted token, Identity)
  identity           one Seeded device identity per test
  fake_tei           a fresh FakeTEI
  app                create_app(settings, db, fake_tei)
  client             httpx.AsyncClient against ``app`` (no auth header)
  authed_client      same, with ``identity``'s bearer token
"""

from __future__ import annotations

import os
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from dataclasses import dataclass, field
from uuid import UUID

ADMIN_DSN = os.environ.get("MEMSVC_TEST_ADMIN_DSN", "postgresql://postgres@127.0.0.1:55432/postgres")
TEST_DB = f"memsvc_test_{os.getpid()}"
TEST_DSN = ADMIN_DSN.rsplit("/", 1)[0] + "/" + TEST_DB

# Must be set before memsvc.config.get_settings() is first called.
os.environ.update(
    DATABASE_URL=TEST_DSN,
    DATABASE_SYSTEM_URL=TEST_DSN,
    FAKE_TEI="1",
    TOKEN_PEPPER="test-pepper",
    RATE_LIMIT_EVENTS_PER_MIN="1000000",
    RATE_LIMIT_DEFAULT_PER_MIN="1000000",
    DB_POOL_MIN="1",
    DB_POOL_MAX="5",
)

import asyncpg  # noqa: E402
import httpx  # noqa: E402
import pytest  # noqa: E402

from memsvc.auth.admin import create_user, ensure_client  # noqa: E402
from memsvc.auth.identity import Identity  # noqa: E402
from memsvc.auth.scopes import KIND_CLIENT_KIND  # noqa: E402
from memsvc.auth.tokens import mint_token, verify_token  # noqa: E402
from memsvc.config import Settings, get_settings  # noqa: E402
from memsvc.db import Database, connect, system_tx  # noqa: E402
from memsvc.embed.tei import FakeTEI  # noqa: E402
from memsvc.ids import uuid7  # noqa: E402
from memsvc.migrate import apply_migrations  # noqa: E402


async def create_test_database(name: str) -> str:
    """(Re)create an empty database on the test cluster; returns its DSN."""
    admin = await asyncpg.connect(ADMIN_DSN)
    try:
        await admin.execute(f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)')
        await admin.execute(f'CREATE DATABASE "{name}"')
    finally:
        await admin.close()
    return ADMIN_DSN.rsplit("/", 1)[0] + "/" + name


async def drop_test_database(name: str) -> None:
    admin = await asyncpg.connect(ADMIN_DSN)
    try:
        await admin.execute(f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)')
    finally:
        await admin.close()


@pytest.fixture
def scratch_database():  # type: ignore[no-untyped-def]
    """``async with scratch_database("suffix") as dsn:`` an empty extra database, dropped afterwards."""

    @asynccontextmanager
    async def _scratch(suffix: str) -> AsyncIterator[str]:
        name = f"{TEST_DB}_{suffix}"
        dsn = await create_test_database(name)
        try:
            yield dsn
        finally:
            await drop_test_database(name)

    return _scratch


@pytest.fixture(scope="session")
def settings() -> Settings:
    return get_settings()


@pytest.fixture(scope="session")
async def db(settings: Settings) -> AsyncIterator[Database]:
    dsn = await create_test_database(TEST_DB)
    conn = await asyncpg.connect(dsn)
    try:
        await apply_migrations(conn, settings.migrations_dir)
    finally:
        await conn.close()
    database = await connect(settings)
    try:
        yield database
    finally:
        await database.close()
        await drop_test_database(TEST_DB)


@dataclass
class Seeded:
    user_id: UUID
    space_id: UUID  # the user's private default space
    client_id: UUID
    device_id: str
    handle: str
    token: str
    token_id: UUID
    identity: Identity
    headers: dict[str, str] = field(default_factory=dict)


@pytest.fixture
def make_identity(db: Database, settings: Settings):  # type: ignore[no-untyped-def]
    """``await make_identity(kind="device"|"runner"|"admin", handle=None, device_id=None, scopes=None)``."""

    async def _make(
        kind: str = "device",
        handle: str | None = None,
        device_id: str | None = None,
        scopes: list[str] | None = None,
    ) -> Seeded:
        suffix = uuid7().hex[-10:]
        handle = handle or f"user-{suffix}"
        device_id = device_id or f"dev-{suffix}"
        async with system_tx(db.system) as conn:
            user = await create_user(conn, handle=handle)
            client_id = await ensure_client(
                conn, user_id=user.user_id, device_id=device_id, kind=KIND_CLIENT_KIND[kind]
            )
            minted = await mint_token(
                conn, user_id=user.user_id, client_id=client_id, kind=kind, pepper=settings.token_pepper, scopes=scopes
            )
        async with db.app.acquire() as conn:
            identity = await verify_token(conn, minted.token, settings.token_pepper)
        return Seeded(
            user_id=user.user_id,
            space_id=user.space_id,
            client_id=client_id,
            device_id=device_id,
            handle=handle,
            token=minted.token,
            token_id=minted.token_id,
            identity=identity,
            headers={"Authorization": f"Bearer {minted.token}", "X-Memory-Device": device_id},
        )

    return _make


@pytest.fixture
async def identity(make_identity) -> Seeded:  # type: ignore[no-untyped-def]
    return await make_identity()


@pytest.fixture
def fake_tei(settings: Settings) -> FakeTEI:
    return FakeTEI(model=settings.embed_model, dims=settings.embed_dims)


@pytest.fixture
def app(settings: Settings, db: Database, fake_tei: FakeTEI):  # type: ignore[no-untyped-def]
    from memsvc.app import create_app

    return create_app(settings=settings, db=db, tei=fake_tei)


@pytest.fixture
async def client(app) -> AsyncIterator[httpx.AsyncClient]:  # type: ignore[no-untyped-def]
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://test") as c:
        yield c


@pytest.fixture
async def authed_client(app, identity: Seeded) -> AsyncIterator[httpx.AsyncClient]:  # type: ignore[no-untyped-def]
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test", headers=identity.headers
    ) as c:
        yield c
