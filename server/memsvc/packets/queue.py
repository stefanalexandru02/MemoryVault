"""Packet creation and the lease protocol (spec 9.1, 9.3, 6.6). Owner: ingest.

``create_packet`` is shared by ingest (extract/summarize) and consolidation (consolidate_*). The lease,
heartbeat and fail helpers run on the caller's ``user_tx`` connection (RLS: only packets in spaces the
caller can write are visible), :func:`expire_leases` on the worker's ``system_tx``.

Attempt counting: ``attempts`` is incremented when a packet is leased (each lease is one attempt), so a
runner that crashes without calling ``/fail`` still uses up an attempt. ``/fail`` releases the lease with a
backoff, or marks the packet ``failed`` when ``attempts >= max_attempts``.
"""

from __future__ import annotations

import logging
from datetime import datetime
from typing import Any
from uuid import UUID

import asyncpg
import jinja2

from memsvc.errors import ApiError
from memsvc.ids import uuid7
from memsvc.prompts import registry
from memsvc.schemas.work import LeasedPacket, PacketPrompt

log = logging.getLogger(__name__)

# Seconds before a released packet is offered again, by /fail error class; doubled per attempt, capped.
BACKOFF_BASE_S = {"quota_exhausted": 900, "cli_missing": 1800, "timeout": 60, "other": 60}
BACKOFF_MAX_S = 6 * 3600
# The failing device cannot run the packet at all: let any runner take it after the backoff.
DROP_PREFERRED_ON = frozenset({"quota_exhausted", "cli_missing"})


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
    """Insert a pending ``work_packets`` row. Returns its id, or None when ``idempotency_key`` exists."""
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


def backoff_seconds(error: str, attempts: int) -> int:
    base = BACKOFF_BASE_S.get(error, BACKOFF_BASE_S["other"])
    return min(BACKOFF_MAX_S, base * 2 ** max(0, attempts - 1))


async def lease(
    conn: asyncpg.Connection,
    *,
    client_id: UUID,
    kinds: list[str],
    max_packets: int,
    lease_seconds: int,
    preferred_runner_seconds: int,
    session_id: UUID | None = None,
) -> list[LeasedPacket]:
    """Lease up to ``max_packets`` packets (spec 6.6, 9.3).

    Order: packets preferring the caller, then unassigned ones (and other clients' packets once
    ``available_after + preferred_runner_seconds`` has passed), then packets whose lease expired; oldest
    first within a group. Concurrent callers never get the same packet (``FOR UPDATE SKIP LOCKED``).
    A packet whose prompt cannot be resolved or rendered is marked ``failed`` and skipped.
    """
    leased: list[LeasedPacket] = []
    while len(leased) < max_packets:
        rows = await conn.fetch(
            """SELECT id FROM work_packets
                WHERE kind = ANY($1::text[])
                  AND ($2::uuid IS NULL OR session_id = $2)
                  AND space_id IN (SELECT writable_spaces())
                  AND attempts < max_attempts
                  AND ((status = 'pending' AND available_after <= now())
                       OR (status = 'leased' AND lease_until < now()))
                  AND (preferred_client_id IS NULL OR preferred_client_id = $3
                       OR available_after + make_interval(secs => $4) <= now())
                ORDER BY CASE WHEN status = 'leased' THEN 2
                              WHEN preferred_client_id = $3 THEN 0 ELSE 1 END,
                         available_after, created_at, id
                LIMIT $5
                FOR UPDATE SKIP LOCKED""",
            kinds,
            session_id,
            client_id,
            float(preferred_runner_seconds),
            max_packets - len(leased),
        )
        if not rows:
            break
        for r in rows:
            packet = await _take(conn, r["id"], client_id, lease_seconds)
            if packet is not None:
                leased.append(packet)
    return leased


async def _take(conn: asyncpg.Connection, packet_id: UUID, client_id: UUID, lease_seconds: int) -> LeasedPacket | None:
    row = await conn.fetchrow(
        """UPDATE work_packets SET status = 'leased', leased_by = $2, lease_until = now() + make_interval(secs => $3),
                  lease_seconds = $3, heartbeats = 0, attempts = attempts + 1
            WHERE id = $1 RETURNING *""",
        packet_id,
        client_id,
        lease_seconds,
    )
    try:
        spec = registry.get(row["prompt_ref"])
        rendered = registry.render(spec, row["inputs"])
    except (KeyError, jinja2.TemplateError) as e:
        log.error("packet %s: cannot render prompt %s: %s", packet_id, row["prompt_ref"], e)
        await settle_failed(conn, packet_id, f"prompt_error: {e}")
        return None
    if spec.prompt_ref != row["prompt_ref"]:  # stored unpinned (registry was empty at creation): pin now
        await conn.execute("UPDATE work_packets SET prompt_ref = $2 WHERE id = $1", packet_id, spec.prompt_ref)
    return LeasedPacket(
        id=row["id"],
        kind=row["kind"],
        prompt_ref=spec.prompt_ref,
        model_hint=row["model_hint"],
        inputs=row["inputs"],
        output_schema=row["output_schema"],
        lease_until=row["lease_until"],
        attempts=row["attempts"],
        prompt=PacketPrompt(system=rendered.system, user_template=spec.user_template, user=rendered.user),
    )


async def lock_packet(conn: asyncpg.Connection, packet_id: UUID) -> asyncpg.Record:
    row = await conn.fetchrow("SELECT *, lease_until < now() AS lease_expired FROM work_packets WHERE id = $1 FOR UPDATE", packet_id)
    if row is None:
        raise ApiError(404, "not_found", "packet not found")
    return row


def require_lease(row: asyncpg.Record, client_id: UUID) -> None:
    """409 unless ``client_id`` holds the packet's lease (an expired lease still counts until re-leased)."""
    if row["status"] != "leased":
        raise ApiError(409, "not_leased", f"packet is {row['status']}, not leased")
    if row["leased_by"] != client_id:
        raise ApiError(409, "lease_conflict", "packet is leased by another client", retryable=False)


async def heartbeat(conn: asyncpg.Connection, packet_id: UUID, client_id: UUID, default_seconds: int) -> asyncpg.Record:
    """Extend the caller's lease once (spec 9.3) by its original duration."""
    row = await lock_packet(conn, packet_id)
    require_lease(row, client_id)
    if row["heartbeats"] >= 1:
        raise ApiError(409, "lease_extension_exhausted", "the lease was already extended once")
    return await conn.fetchrow(
        """UPDATE work_packets SET lease_until = now() + make_interval(secs => $2), heartbeats = heartbeats + 1
            WHERE id = $1 RETURNING id, lease_until""",
        packet_id,
        row["lease_seconds"] or default_seconds,
    )


async def fail(conn: asyncpg.Connection, packet_id: UUID, client_id: UUID, error: str, detail: str | None) -> asyncpg.Record:
    """Release the caller's lease after a runner failure (spec 6.6). Returns the updated row."""
    row = await lock_packet(conn, packet_id)
    require_lease(row, client_id)
    message = f"{error}: {detail}" if detail else error
    if row["attempts"] >= row["max_attempts"]:
        return await settle_failed(conn, packet_id, message)
    return await conn.fetchrow(
        """UPDATE work_packets SET status = 'pending', lease_until = NULL, last_error = $2,
                  available_after = now() + make_interval(secs => $3),
                  preferred_client_id = CASE WHEN $4 THEN NULL ELSE preferred_client_id END
            WHERE id = $1 RETURNING *""",
        packet_id,
        message,
        float(backoff_seconds(error, row["attempts"])),
        error in DROP_PREFERRED_ON,
    )


async def settle_failed(conn: asyncpg.Connection, packet_id: UUID, message: str) -> asyncpg.Record:
    """Mark a packet ``failed`` and trigger what waited on it (deferred summaries)."""
    from memsvc.packets.windows import after_packet_settled

    row = await conn.fetchrow(
        """UPDATE work_packets SET status = 'failed', lease_until = NULL, last_error = $2, done_at = now()
            WHERE id = $1 RETURNING *""",
        packet_id,
        message,
    )
    await after_packet_settled(conn, row["kind"], row["session_id"], row["inputs"])
    return row


async def expire_leases(conn: asyncpg.Connection) -> int:
    """Fail packets whose lease expired on their last attempt (worker sweep). Returns how many."""
    rows = await conn.fetch(
        """SELECT id FROM work_packets
            WHERE status = 'leased' AND lease_until < now() AND attempts >= max_attempts
            FOR UPDATE SKIP LOCKED"""
    )
    for r in rows:
        await settle_failed(conn, r["id"], "lease expired on the last attempt")
    return len(rows)
