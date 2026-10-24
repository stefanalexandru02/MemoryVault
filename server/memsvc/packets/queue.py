"""Packet creation shared by ingest (extract/summarize) and consolidation (consolidate_*). Owner: ingest."""

from __future__ import annotations

from datetime import datetime
from typing import Any
from uuid import UUID

import asyncpg

from memsvc.ids import uuid7


async def create_packet(
    conn: asyncpg.Connection,
    *,
    space_id: UUID,
    kind: str,
    prompt_ref: str,
    model_hint: str,
    output_schema: str,
    inputs: dict[str, Any],
    idempotency_key: str,
    session_id: UUID | None = None,
    preferred_client_id: UUID | None = None,
    available_after: datetime | None = None,
    max_attempts: int = 3,
) -> UUID | None:
    """Insert a pending ``work_packets`` row. Returns its id, or None when ``idempotency_key`` exists.

    Minimal working default written by the foundation; the owner may extend it but keeps the signature.
    """
    row = await conn.fetchrow(
        """INSERT INTO work_packets (id, space_id, kind, session_id, preferred_client_id, prompt_ref, model_hint,
                                    inputs, output_schema, idempotency_key, available_after, max_attempts)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, coalesce($11, now()), $12)
           ON CONFLICT (idempotency_key) DO NOTHING
           RETURNING id""",
        uuid7(),
        space_id,
        kind,
        session_id,
        preferred_client_id,
        prompt_ref,
        model_hint,
        inputs,
        output_schema,
        idempotency_key,
        available_after,
        max_attempts,
    )
    return row["id"] if row else None
