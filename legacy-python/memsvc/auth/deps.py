"""FastAPI dependencies: bearer auth, scope checks, per-token rate limiting (spec 5.1-5.3).

Usage in a router::

    @router.post("/v1/sessions")
    async def create(body: SessionCreate, request: Request,
                     identity: Identity = Depends(require_scope(EVENTS_WRITE))): ...

``request.app.state.db`` (:class:`memsvc.db.Database`) and ``request.app.state.settings`` are set by
``create_app``. The MCP app, which is not a FastAPI router, calls :func:`authenticate_header` and
:func:`check_rate_limit` directly.
"""

from __future__ import annotations

import logging
from collections.abc import Awaitable, Callable

import asyncpg
from fastapi import Request

from memsvc.auth.identity import Identity
from memsvc.auth.scopes import EVENTS_WRITE
from memsvc.auth.tokens import verify_token
from memsvc.config import Settings
from memsvc.db import Database
from memsvc.errors import ApiError

log = logging.getLogger(__name__)


async def authenticate_header(db: Database, settings: Settings, authorization: str | None) -> Identity:
    """Resolve an ``Authorization: Bearer mem_...`` header value, or raise ApiError(401)."""
    if not authorization or not authorization[:7].lower() == "bearer ":
        raise ApiError(401, "unauthorized", "missing bearer token", headers={"WWW-Authenticate": "Bearer"})
    async with db.app.acquire() as conn:
        return await verify_token(conn, authorization[7:].strip(), settings.token_pepper)


async def check_rate_limit(pool: asyncpg.Pool, subject: str, limit: int) -> None:
    """Fixed one-minute window counter in ``rate_limits``. Raises 429 with Retry-After; fails open on DB error."""
    try:
        row = await pool.fetchrow(
            """INSERT INTO rate_limits (subject, window_start, count) VALUES ($1, date_trunc('minute', now()), 1)
               ON CONFLICT (subject, window_start) DO UPDATE SET count = rate_limits.count + 1
               RETURNING count,
                 greatest(1, ceil(extract(epoch FROM window_start + interval '1 minute' - now())))::int
                   AS retry_after""",
            subject,
        )
    except Exception:  # noqa: BLE001 - fail open (spec 5.1)
        log.warning("rate limit check failed; allowing request", exc_info=True)
        return
    if row is not None and row["count"] > limit:
        raise ApiError(
            429,
            "rate_limited",
            f"rate limit of {limit}/min exceeded",
            retryable=True,
            headers={"Retry-After": str(row["retry_after"])},
        )


def rate_limit_for(settings: Settings, bucket: str) -> int:
    return settings.rate_limit_events_per_min if bucket == "events" else settings.rate_limit_default_per_min


async def get_identity(request: Request) -> Identity:
    """Authenticate the request (once per request; cached on ``request.state.identity``)."""
    cached = getattr(request.state, "identity", None)
    if cached is not None:
        return cached
    identity = await authenticate_header(
        request.app.state.db, request.app.state.settings, request.headers.get("authorization")
    )
    request.state.identity = identity
    return identity


def require_scope(*scopes: str) -> Callable[[Request], Awaitable[Identity]]:
    """Dependency factory: authenticate, require every scope in ``scopes`` (403 otherwise), rate limit.

    Routes requiring ``events:write`` use the events bucket (600/min); all others the default (120/min).
    """
    bucket = "events" if EVENTS_WRITE in scopes else "default"

    async def dependency(request: Request) -> Identity:
        identity = await get_identity(request)
        missing = [s for s in scopes if s not in identity.scopes]
        if missing:
            raise ApiError(403, "insufficient_scope", f"token lacks scope {missing[0]}")
        settings: Settings = request.app.state.settings
        await check_rate_limit(
            request.app.state.db.app, f"token:{identity.token_id}:{bucket}", rate_limit_for(settings, bucket)
        )
        return identity

    return dependency
