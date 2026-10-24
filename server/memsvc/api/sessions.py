"""Spec 6.1: POST /v1/sessions, POST /v1/sessions/{id}/end (events:write). Owner: ingest."""

from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends, Request
from fastapi.responses import JSONResponse

from memsvc.auth import EVENTS_WRITE, Identity, require_scope, resolve_write_space
from memsvc.db import user_tx
from memsvc.errors import ApiError
from memsvc.ids import uuid7
from memsvc.packets import windows
from memsvc.schemas.sessions import SessionCreate, SessionCreated, SessionEnd, SessionEnded, WindowPolicy

router = APIRouter()


def window_policy(request: Request) -> WindowPolicy:
    s = request.app.state.settings
    return WindowPolicy(tool_uses=s.window_tool_uses, idle_seconds=s.window_idle_seconds)


async def lock_own_session(conn, identity: Identity, session_id: UUID):  # type: ignore[no-untyped-def]
    """Lock a session the caller's client owns: 404 when invisible, 403 when another client's."""
    session = await windows.lock_session(conn, session_id)
    if session is None:
        raise ApiError(404, "not_found", "session not found")
    if session["client_id"] != identity.client_id:
        raise ApiError(403, "forbidden", "session belongs to another client")
    return session


@router.post("/v1/sessions", response_model=SessionCreated, status_code=201)
async def create_session(
    body: SessionCreate, request: Request, identity: Identity = Depends(require_scope(EVENTS_WRITE))
) -> JSONResponse:
    """Idempotent on ``(client, external_session_id)``: a repeat returns 200 with the existing id."""
    async with user_tx(request.app.state.db.app, identity.user_id, identity.client_id) as conn:
        space_id = await resolve_write_space(conn, identity, body.space_id)
        row = await conn.fetchrow(
            """INSERT INTO sessions (id, space_id, client_id, harness, external_session_id, project_key, cwd, metadata)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
               ON CONFLICT (client_id, external_session_id) DO NOTHING
               RETURNING id, space_id""",
            uuid7(),
            space_id,
            identity.client_id,
            body.harness,
            body.external_session_id,
            body.project_key,
            body.cwd,
            body.metadata,
        )
        status = 201
        if row is None:
            status = 200
            row = await conn.fetchrow(
                "SELECT id, space_id FROM sessions WHERE client_id = $1 AND external_session_id = $2",
                identity.client_id,
                body.external_session_id,
            )
            if row is None:  # exists in a space this user can no longer see
                raise ApiError(409, "conflict", "session exists but is not visible to this user")
        if body.project_key:
            await conn.execute(
                """UPDATE clients SET current_project_key = $2, current_project_at = now(), last_seen_at = now()
                    WHERE id = $1""",
                identity.client_id,
                body.project_key,
            )
    out = SessionCreated(session_id=row["id"], space_id=row["space_id"], window_policy=window_policy(request))
    return JSONResponse(out.model_dump(mode="json"), status_code=status)


@router.post("/v1/sessions/{session_id}/end", response_model=SessionEnded)
async def end_session(
    session_id: UUID,
    request: Request,
    body: SessionEnd | None = None,
    identity: Identity = Depends(require_scope(EVENTS_WRITE)),
) -> SessionEnded:
    """Close the open window (creating its extract packet) and owe ``summarize_session`` (9.1)."""
    async with user_tx(request.app.state.db.app, identity.user_id, identity.client_id) as conn:
        session = await lock_own_session(conn, identity, session_id)
        closed, created = await windows.end_session(conn, session, body.reason if body else None)
        ended_at = await conn.fetchval("SELECT ended_at FROM sessions WHERE id = $1", session_id)
    return SessionEnded(session_id=session_id, ended_at=ended_at, window_closed=closed, packets_created=created)
