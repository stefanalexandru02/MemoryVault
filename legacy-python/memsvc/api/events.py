"""Spec 6.2, 9.1: POST /v1/sessions/{id}/events (events:write): clamp, window assignment, extract packets.

Owner: ingest. Every event is inserted with ``ON CONFLICT DO NOTHING`` (``idempotency_key`` and
``(session_id, seq)`` are unique), so replaying a spool file is a no-op that reports duplicates.
"""

from __future__ import annotations

from typing import Any
from uuid import UUID

from fastapi import APIRouter, Depends, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from pydantic import ValidationError

from memsvc.api.sessions import lock_own_session
from memsvc.auth import EVENTS_WRITE, Identity, require_scope
from memsvc.db import json_dumps, user_tx
from memsvc.errors import ApiError
from memsvc.ids import uuid7
from memsvc.packets import windows
from memsvc.schemas.common import estimate_tokens
from memsvc.schemas.events import (
    FIELD_HEAD_CHARS,
    FIELD_TAIL_CHARS,
    MAX_BODY_BYTES,
    PAYLOAD_CAP_BYTES,
    EventsAccepted,
    EventsBatch,
)

router = APIRouter()

MIN_HEAD_CHARS = 64
EXCERPT_CHARS = 8000  # last-resort excerpt; even fully escaped it stays far below the 64 KB cap


def clamp_strings(value: Any, head: int = FIELD_HEAD_CHARS, tail: int = FIELD_TAIL_CHARS) -> Any:
    """Every string longer than head + tail becomes ``head …[clamped N chars]… tail`` (recursively)."""
    if isinstance(value, str):
        if len(value) <= head + tail:
            return value
        cut = len(value) - head - tail
        return f"{value[:head]}…[clamped {cut} chars]…{value[len(value) - tail :] if tail else ''}"
    if isinstance(value, dict):
        return {k: clamp_strings(v, head, tail) for k, v in value.items()}
    if isinstance(value, list):
        return [clamp_strings(v, head, tail) for v in value]
    return value


def clamp_payload(payload: dict[str, Any]) -> tuple[dict[str, Any], str]:
    """Spec 6.2 clamp: per-string head/tail, then a 64 KB cap on the whole payload. Returns (payload, json).

    Over the cap the per-string limits are halved until it fits; a payload that still does not fit (huge
    arrays, many keys) keeps its short top-level scalars plus a JSON excerpt.
    """
    head, tail = FIELD_HEAD_CHARS, FIELD_TAIL_CHARS
    out = clamp_strings(payload, head, tail)
    text = json_dumps(out)
    while len(text.encode()) > PAYLOAD_CAP_BYTES and head > MIN_HEAD_CHARS:
        head, tail = head // 2, tail // 2
        out = clamp_strings(payload, head, tail)
        text = json_dumps(out)
    if len(text.encode()) > PAYLOAD_CAP_BYTES:
        out = {
            k: v
            for k, v in payload.items()
            if isinstance(v, int | float | bool) or v is None or (isinstance(v, str) and len(v) <= 256)
        }
        out["payload_clamped"] = True
        out["excerpt"] = text[:EXCERPT_CHARS]
        text = json_dumps(out)
    return out, text


async def _read_body(request: Request) -> bytes:
    length = request.headers.get("content-length")
    if length and length.isdigit() and int(length) > MAX_BODY_BYTES:
        raise ApiError(413, "payload_too_large", f"body exceeds {MAX_BODY_BYTES} bytes")
    chunks, size = [], 0
    async for chunk in request.stream():
        size += len(chunk)
        if size > MAX_BODY_BYTES:
            raise ApiError(413, "payload_too_large", f"body exceeds {MAX_BODY_BYTES} bytes")
        chunks.append(chunk)
    return b"".join(chunks)


@router.post("/v1/sessions/{session_id}/events", response_model=EventsAccepted, status_code=202)
async def post_events(
    session_id: UUID, request: Request, identity: Identity = Depends(require_scope(EVENTS_WRITE))
) -> JSONResponse:
    """Body: ``EventsBatch`` (read by hand to enforce the 4 MB limit before parsing)."""
    raw = await _read_body(request)
    try:
        batch = EventsBatch.model_validate_json(raw)
    except ValidationError as e:
        raise RequestValidationError(e.errors(include_url=False, include_context=False)) from e

    ids, seqs, kinds, payloads, keys, times, tokens = [], [], [], [], [], [], []
    for ev in batch.events:
        payload, text = clamp_payload(ev.payload)
        ids.append(uuid7())
        seqs.append(ev.seq)
        kinds.append(ev.kind)
        payloads.append(payload)
        keys.append(ev.idempotency_key)
        times.append(ev.occurred_at)
        tokens.append(estimate_tokens(text))

    async with user_tx(request.app.state.db.app, identity.user_id, identity.client_id) as conn:
        session = await lock_own_session(conn, identity, session_id)
        inserted = await conn.fetch(
            """INSERT INTO events (id, space_id, session_id, seq, kind, payload, idempotency_key, occurred_at,
                                  token_estimate)
               SELECT u.id, $1, $2, u.seq, u.kind, u.payload, u.key, u.at, u.tokens
                 FROM unnest($3::uuid[], $4::int[], $5::text[], $6::jsonb[], $7::text[], $8::timestamptz[], $9::int[])
                      AS u(id, seq, kind, payload, key, at, tokens)
               ON CONFLICT DO NOTHING
               RETURNING id, seq, kind""",
            session["space_id"],
            session_id,
            ids,
            seqs,
            kinds,
            payloads,
            keys,
            times,
            tokens,
        )
        new = [windows.NewEvent(id=r["id"], seq=r["seq"], kind=r["kind"]) for r in inserted]
        assignment = await windows.assign_events(conn, session, new)
        await conn.execute(
            """UPDATE clients SET last_seen_at = now(),
                      current_project_key = coalesce($2, current_project_key),
                      current_project_at = CASE WHEN $2::text IS NULL THEN current_project_at ELSE now() END
                WHERE id = $1""",
            identity.client_id,
            session["project_key"],
        )
    out = EventsAccepted(
        accepted=len(inserted),
        duplicates=len(batch.events) - len(inserted),
        window_no=assignment.window_no,
        window_closed=bool(assignment.closed),
    )
    return JSONResponse(out.model_dump(mode="json"), status_code=202)
