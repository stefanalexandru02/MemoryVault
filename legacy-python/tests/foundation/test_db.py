from __future__ import annotations

import asyncio
from uuid import UUID

import asyncpg

from memsvc.db import Database, caps, system_tx, tune_vector_search, user_tx
from memsvc.ids import uuid7
from memsvc.invalidate import CHANNEL, notify_invalidate, parse_payload


async def test_user_tx_sets_role_and_settings(db: Database, identity) -> None:  # type: ignore[no-untyped-def]
    async with user_tx(db.app, identity.user_id, identity.client_id) as conn:
        row = await conn.fetchrow("SELECT current_user AS u, app_user_id() AS uid, app_client_id() AS cid")
    assert row["u"] == "memsvc_app" and row["uid"] == identity.user_id and row["cid"] == identity.client_id
    async with db.app.acquire() as conn:  # SET LOCAL does not leak past the transaction
        assert await conn.fetchval("SELECT app_user_id()") is None


async def test_system_tx_role(db: Database) -> None:
    async with system_tx(db.system) as conn:
        assert await conn.fetchval("SELECT current_user") == "memsvc_system"


async def test_caps_and_vector_tuning(db: Database) -> None:
    async with db.app.acquire() as conn:
        c = caps(conn)
        assert c.superuser and c.pgvector_version is not None
        assert c.hnsw_iterative_scan == (c.pgvector_version >= (0, 8))
        async with conn.transaction():
            await tune_vector_search(conn)
            assert await conn.fetchval("SHOW hnsw.ef_search") == "80"


async def test_codecs_roundtrip(db: Database, identity) -> None:  # type: ignore[no-untyped-def]
    oid = uuid7()
    vec = [i / 1024 for i in range(1024)]
    async with user_tx(db.app, identity.user_id, identity.client_id) as conn:
        await conn.execute(
            "INSERT INTO embeddings (space_id, owner_table, owner_id, part, model, embedding, scope_kind, scope_key) "
            "VALUES ($1, 'observations', $2, 'title', 'm', $3, 'global', '')",
            identity.space_id,
            oid,
            vec,
        )
        got = await conn.fetchval("SELECT embedding FROM embeddings WHERE owner_id = $1", oid)
        meta = await conn.fetchval("SELECT $1::jsonb", {"id": oid, "nested": [1, {"a": None}]})
    assert len(got) == 1024 and abs(got[512] - 0.5) < 1e-6
    assert meta == {"id": str(oid), "nested": [1, {"a": None}]}


async def test_notify_invalidate_delivered_on_commit(db: Database, settings) -> None:  # type: ignore[no-untyped-def]
    received: asyncio.Queue[str] = asyncio.Queue()
    listener = await asyncpg.connect(settings.database_url)
    try:
        await listener.add_listener(CHANNEL, lambda *args: received.put_nowait(args[3]))
        space = uuid7()
        async with system_tx(db.system) as conn:
            await notify_invalidate(conn, space, "project", "github.com/a/b:c")
            await asyncio.sleep(0.05)
            assert received.empty()  # not before commit
        payload = await asyncio.wait_for(received.get(), timeout=2)
    finally:
        await listener.close()
    assert parse_payload(payload) == (UUID(str(space)), "project", "github.com/a/b:c")
