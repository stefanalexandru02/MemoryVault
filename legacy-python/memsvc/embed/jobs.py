"""Embed job queue (spec 13). Owner: worker.

Contract: ``jobs(type='embed', payload={"owner_table": <table>, "owner_id": "<uuid>"})``. The worker
drains them in batches (<= 64 texts per TEI call), writes ``embeddings`` rows for every part from
``embed.parts.parts_for`` with the row's ``space_id``/``scope_kind``/``scope_key``.
"""

from __future__ import annotations

from uuid import UUID

import asyncpg

from memsvc.ids import uuid7


async def enqueue_embed(conn: asyncpg.Connection, table: str, id: UUID) -> None:
    """Queue (re-)embedding of one row. Call in the transaction that writes the row.

    Minimal working default written by the foundation; the owner may extend it but keeps the signature.
    """
    await conn.execute(
        "INSERT INTO jobs (id, type, payload) VALUES ($1, 'embed', $2)",
        uuid7(),
        {"owner_table": table, "owner_id": str(id)},
    )
