"""SessionStart context block (spec 6.3 GET /v1/context, 11.5, 11.6, 11.7). Owner: recall."""

from __future__ import annotations

import asyncpg

from memsvc.auth.identity import Identity


async def render_context(
    conn: asyncpg.Connection, identity: Identity, project_key: str | None, device_id: str | None, source: str
) -> str:
    """Markdown block for SessionStart: profile, since-you-were-here (live), open work, procedures,
    recent observation index with legend. Cached portions come from ``context_cache`` (recall.cache).
    ``source='resume'`` returns an empty string. Runs in ``user_tx``."""
    raise NotImplementedError
