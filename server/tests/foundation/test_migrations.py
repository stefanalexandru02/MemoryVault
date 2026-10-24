from __future__ import annotations

import asyncpg

from memsvc.config import Settings
from memsvc.db import Database
from memsvc.migrate import apply_migrations, discover

EXPECTED_TABLES = {
    "users", "spaces", "memberships", "clients", "api_tokens", "sessions", "events", "observations", "summaries",
    "entities", "facts", "procedures", "work_state", "profiles", "embeddings", "work_packets", "jobs",
    "memory_audit", "retrieval_log", "access_log", "procedure_hints", "context_cache", "rate_limits",
    "schema_migrations",
}  # fmt: skip


async def test_rerun_is_noop(db: Database, settings: Settings) -> None:
    async with db.app.acquire() as conn:
        assert await apply_migrations(conn, settings.migrations_dir) == []
        versions = [r["version"] for r in await conn.fetch("SELECT version FROM schema_migrations ORDER BY version")]
    assert versions == [m.version for m in discover(settings.migrations_dir)]


async def test_fresh_database_twice(settings: Settings, scratch_database) -> None:  # type: ignore[no-untyped-def]
    async with scratch_database("mig") as dsn:
        conn = await asyncpg.connect(dsn)
        try:
            first = await apply_migrations(conn, settings.migrations_dir)
            second = await apply_migrations(conn, settings.migrations_dir)
            tables = {
                r["tablename"] for r in await conn.fetch("SELECT tablename FROM pg_tables WHERE schemaname='public'")
            }
        finally:
            await conn.close()
    assert first and first == sorted(first)
    assert second == []
    assert EXPECTED_TABLES <= tables


async def test_schema_details(db: Database) -> None:
    async with db.app.acquire() as conn:
        cols = {
            (r["table_name"], r["column_name"])
            for r in await conn.fetch(
                "SELECT table_name, column_name FROM information_schema.columns WHERE table_schema = 'public'"
            )
        }
        assert ("clients", "current_project_key") in cols
        assert ("events", "payload_trimmed") in cols
        indexes = {
            r["indexname"] for r in await conn.fetch("SELECT indexname FROM pg_indexes WHERE schemaname='public'")
        }
        assert {"emb_hnsw", "facts_one_active"} <= indexes
        pg_search = await conn.fetchval("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_search')")
        assert ("obs_bm25" in indexes) == pg_search
        roles = {r["rolname"]: r for r in await conn.fetch("SELECT * FROM pg_roles WHERE rolname LIKE 'memsvc_%'")}
        assert roles["memsvc_system"]["rolbypassrls"] and not roles["memsvc_app"]["rolbypassrls"]


async def test_generated_search_columns(db: Database, make_identity) -> None:  # type: ignore[no-untyped-def]
    from memsvc.db import system_tx
    from memsvc.ids import uuid7

    seeded = await make_identity()
    async with system_tx(db.system) as conn:
        oid = uuid7()
        await conn.execute(
            """INSERT INTO observations (id, space_id, scope_kind, scope_key, type, title, narrative, facts,
                                        extracted_by, content_hash)
               VALUES ($1, $2, 'project', 'p', 'decision', 'Chose pgvector', 'over Chroma', $3, $4, 'h')""",
            oid,
            seeded.space_id,
            ["one store", "RLS covers vectors"],
            {"runner_kind": "explicit"},
        )
        row = await conn.fetchrow(
            "SELECT search_text, search_tsv @@ websearch_to_tsquery('simple', 'chroma vectors') AS hit "
            "FROM observations WHERE id = $1",
            oid,
        )
    assert row["search_text"] == "Chose pgvector  over Chroma one store RLS covers vectors"
    assert row["hit"]
