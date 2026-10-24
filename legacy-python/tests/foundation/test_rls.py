"""Spec 4.7 / 16.1: a second user's identity sees zero rows in every space-scoped table."""

from __future__ import annotations

from uuid import UUID

import asyncpg
import pytest

from memsvc.auth.admin import add_membership, create_space
from memsvc.db import Database, system_tx, user_tx
from memsvc.ids import uuid7


async def space_scoped_tables(conn: asyncpg.Connection) -> list[str]:
    rows = await conn.fetch(
        """SELECT c.table_name FROM information_schema.columns c
             JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name
            WHERE c.table_schema = 'public' AND c.column_name = 'space_id' AND t.table_type = 'BASE TABLE'
            ORDER BY 1"""
    )
    return [r["table_name"] for r in rows]


async def seed_space(conn: asyncpg.Connection, space_id: UUID, user_id: UUID, client_id: UUID) -> set[str]:
    """One row in every foundation space-scoped table. Returns the tables seeded."""
    sid, oid, eid, fid, pid, wid = (uuid7() for _ in range(6))
    await conn.execute(
        "INSERT INTO sessions (id, space_id, client_id, harness, external_session_id, project_key) "
        "VALUES ($1, $2, $3, 'claude-code', $4, 'local/p')",
        sid, space_id, client_id, str(sid),
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO events (id, space_id, session_id, seq, kind, payload, idempotency_key, occurred_at) "
        "VALUES ($1, $2, $3, 1, 'user_prompt', $4, $5, now())",
        uuid7(), space_id, sid, {"prompt": "hi"}, f"{sid}:1",
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO observations (id, space_id, scope_kind, scope_key, session_id, type, title, extracted_by, "
        "content_hash) VALUES ($1, $2, 'project', 'local/p', $3, 'decision', 'secret plan', $4, 'h')",
        oid, space_id, sid, {"runner_kind": "explicit"},
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, session_id, next_steps, extracted_by) "
        "VALUES ($1, $2, 'project', 'local/p', 'session', $3, 'ship it', '{}')",
        uuid7(), space_id, sid,
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO entities (id, space_id, kind, canonical_name) VALUES ($1, $2, 'user', 'alex')", eid, space_id
    )
    await conn.execute(
        "INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text, status) "
        "VALUES ($1, $2, 'global', '', $3, 'prefers.editor', '\"helix\"', 'helix', 'active')",
        fid, space_id, eid,
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO procedures (id, space_id, scope_kind, scope_key, name, task_class, trigger_text, body_md) "
        "VALUES ($1, $2, 'project', 'local/p', 'Deploy', 'deploy', 'when deploying', 'run migrations')",
        pid, space_id,
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO work_state (id, space_id, scope_kind, scope_key, list_name, fields) "
        "VALUES ($1, $2, 'project', 'local/p', 'todo', '{\"a\": \"done\"}')",
        uuid7(), space_id,
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO profiles (space_id, scope_kind, scope_key, body_md, token_estimate, facts_version) "
        "VALUES ($1, 'global', '', '- helix', 2, 1)",
        space_id,
    )
    await conn.execute(
        "INSERT INTO embeddings (space_id, owner_table, owner_id, part, model, embedding, scope_kind, scope_key) "
        "VALUES ($1, 'observations', $2, 'title', 'bge-m3', $3, 'project', 'local/p')",
        space_id, oid, [0.1] * 1024,
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO work_packets (id, space_id, kind, session_id, prompt_ref, model_hint, inputs, output_schema, "
        "idempotency_key) VALUES ($1, $2, 'extract', $3, 'observer.code@v3', 'sonnet', '{}', 'observer.v1', $4)",
        wid, space_id, sid, f"extract:{sid}:1",
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO memory_audit (id, space_id, actor, action, target_table, target_id) "
        "VALUES ($1, $2, 'planner', 'fact.create', 'facts', $3)",
        uuid7(), space_id, fid,
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO retrieval_log (id, space_id, client_id, mode, scopes, candidates, selected, latency_ms, reranked) "
        "VALUES ($1, $2, $3, 'push', '[]', '[]', '[]', 5, false)",
        uuid7(), space_id, client_id,
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO access_log (id, space_id, client_id, action, target_ids) VALUES ($1, $2, $3, 'get', $4)",
        uuid7(), space_id, client_id, [oid],
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO procedure_hints (id, space_id, scope_kind, scope_key, task_class, note, observation_id) "
        "VALUES ($1, $2, 'project', 'local/p', 'deploy', 'run migrations first', $3)",
        uuid7(), space_id, oid,
    )  # fmt: skip
    await conn.execute(
        "INSERT INTO context_cache (cache_key, user_id, space_id, space_ids, scopes, body_md, etag) "
        "VALUES ($1, $2, $3, $4, $5, '## Memory', 'e1')",
        f"k:{space_id}", user_id, space_id, [space_id], ["global:", "project:local/p"],
    )  # fmt: skip
    return {
        "sessions", "events", "observations", "summaries", "entities", "facts", "procedures", "work_state",
        "profiles", "embeddings", "work_packets", "memory_audit", "retrieval_log", "access_log",
        "procedure_hints", "context_cache",
    }  # fmt: skip


async def test_every_space_table_has_forced_rls(db: Database) -> None:
    async with db.app.acquire() as conn:
        tables = await space_scoped_tables(conn)
        assert "embeddings" in tables and "observations" in tables
        for t in tables:
            row = await conn.fetchrow(
                "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE oid = $1::regclass", t
            )
            assert row["relrowsecurity"] and row["relforcerowsecurity"], f"{t}: RLS not enabled and forced"
            n = await conn.fetchval(
                "SELECT count(*) FROM pg_policies WHERE schemaname = 'public' AND tablename = $1 "
                "AND 'memsvc_app' = ANY (roles)",
                t,
            )
            assert n >= 1, f"{t}: no policy for memsvc_app"


async def test_second_user_sees_nothing(db: Database, make_identity) -> None:  # type: ignore[no-untyped-def]
    alice, bob = await make_identity(), await make_identity()
    async with system_tx(db.system) as conn:
        seeded = await seed_space(conn, alice.space_id, alice.user_id, alice.client_id)
        tables = await space_scoped_tables(conn)
    assert seeded <= set(tables), set(seeded) - set(tables)

    async with user_tx(db.app, alice.user_id, alice.client_id) as conn:
        for t in seeded:
            n = await conn.fetchval(f"SELECT count(*) FROM {t} WHERE space_id = $1", alice.space_id)
            assert n >= 1, f"owner cannot see own rows in {t}"

    async with user_tx(db.app, bob.user_id, bob.client_id) as conn:
        assert await conn.fetchval("SELECT current_user") == "memsvc_app"
        for t in tables:
            if t == "memberships":  # bob sees exactly his own membership rows
                assert await conn.fetchval("SELECT count(*) FROM memberships WHERE user_id <> $1", bob.user_id) == 0
                continue
            assert await conn.fetchval(f"SELECT count(*) FROM {t}") == 0, f"{t} leaks rows to another user"
        # the vector leg itself is filtered by the policy
        assert await conn.fetchval("SELECT count(*) FROM embeddings ORDER BY 1 LIMIT 5") == 0
        # updates and deletes silently match nothing
        assert await conn.execute("UPDATE observations SET title = 'x'") == "UPDATE 0"
        assert await conn.execute("DELETE FROM facts") == "DELETE 0"

    # no app.user_id at all: nothing visible either
    async with db.app.acquire() as conn, conn.transaction():
        await conn.execute("SET LOCAL ROLE memsvc_app")
        assert await conn.fetchval("SELECT count(*) FROM observations") == 0


async def test_second_user_cannot_write_into_foreign_space(db: Database, make_identity) -> None:  # type: ignore[no-untyped-def]
    alice, bob = await make_identity(), await make_identity()
    with pytest.raises(asyncpg.InsufficientPrivilegeError):
        async with user_tx(db.app, bob.user_id, bob.client_id) as conn:
            await conn.execute(
                "INSERT INTO observations (id, space_id, scope_kind, scope_key, type, title, extracted_by, "
                "content_hash) VALUES ($1, $2, 'global', '', 'change', 't', '{}', 'h')",
                uuid7(),
                alice.space_id,
            )


async def test_shared_space_reader(db: Database, make_identity) -> None:  # type: ignore[no-untyped-def]
    alice, carol = await make_identity(), await make_identity()
    async with system_tx(db.system) as conn:
        shared = await create_space(conn, name="team", kind="shared", owner_id=alice.user_id)
        await add_membership(conn, space_id=shared, user_id=carol.user_id, role="reader")
        await seed_space(conn, shared, alice.user_id, alice.client_id)

    async with user_tx(db.app, carol.user_id, carol.client_id) as conn:
        assert await conn.fetchval("SELECT count(*) FROM observations WHERE space_id = $1", shared) == 1
        assert await conn.fetchval("SELECT count(*) FROM embeddings WHERE space_id = $1", shared) == 1
        # readers may append to logs ...
        await conn.execute(
            "INSERT INTO access_log (id, space_id, client_id, action, target_ids) VALUES ($1, $2, $3, 'get', '{}')",
            uuid7(),
            shared,
            carol.client_id,
        )
        # ... but context_cache rows are per user
        assert await conn.fetchval("SELECT count(*) FROM context_cache") == 0
        assert await conn.execute("UPDATE observations SET title = 'x'") == "UPDATE 0"

    with pytest.raises(asyncpg.InsufficientPrivilegeError):
        async with user_tx(db.app, carol.user_id, carol.client_id) as conn:
            await conn.execute(
                "INSERT INTO work_state (id, space_id, scope_kind, scope_key, list_name, fields) "
                "VALUES ($1, $2, 'global', '', 'todo', '{}')",
                uuid7(),
                shared,
            )


async def test_invalidate_function_crosses_users(db: Database, make_identity) -> None:  # type: ignore[no-untyped-def]
    alice = await make_identity()
    async with system_tx(db.system) as conn:
        await seed_space(conn, alice.space_id, alice.user_id, alice.client_id)
    # runs as memsvc_app with no user context, like the API's LISTEN loop
    async with db.app.acquire() as conn, conn.transaction():
        await conn.execute("SET LOCAL ROLE memsvc_app")
        sql = "SELECT memsvc_invalidate_context($1, $2, $3)"
        assert await conn.fetchval(sql, alice.space_id, "project", "other") == 0
        assert await conn.fetchval(sql, alice.space_id, "project", "local/p") == 1
