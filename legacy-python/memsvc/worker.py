"""Worker / broker loop (spec 9.3, 13, 14): embed jobs, lease-expiry and idle-window sweeps every
30 s (``packets.windows.close_idle_windows``), context re-render (``recall.cache.rerender``),
retention (spec 4.8, also prunes old ``rate_limits`` rows). Owner: worker. Runs as ``memsvc worker``.
"""

from __future__ import annotations

from memsvc.config import Settings


async def run_worker(settings: Settings) -> None:
    """Run until cancelled. Uses the system pool (``DATABASE_SYSTEM_URL``)."""
    raise NotImplementedError
