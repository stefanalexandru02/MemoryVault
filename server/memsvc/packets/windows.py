"""Window assignment and closing (spec 9.1). Owner: ingest."""

from __future__ import annotations

import asyncpg


async def close_idle_windows(conn: asyncpg.Connection) -> int:
    """Close every open window whose newest event is older than ``window_idle_seconds`` (90 s).

    Called by the worker sweep every 30 s inside ``system_tx``. Closing a window with content creates
    its ``extract`` packet (idempotency key ``extract:<session>:<window>``); windows holding only
    ``user_prompt``/``system`` events create none. Returns the number of windows closed.
    """
    raise NotImplementedError
