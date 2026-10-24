"""Result appliers, keyed by packet kind (spec 9.4, 10.4, 10.5, 12.2, 12.3).

Ingest registers ``extract``, ``summarize_prompt``, ``summarize_session``; consolidation registers the
``consolidate_*`` kinds (at import of its module). The work-result route validates the output with
``memsvc.schemas.outputs.validate_output`` and then calls :func:`apply` in the same transaction that
marks the packet done. An applier returns counts for the response, e.g. ``{"observations": 2}``.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable

import asyncpg
from pydantic import BaseModel

from memsvc.schemas.work import PacketRecord

Applier = Callable[[asyncpg.Connection, PacketRecord, BaseModel], Awaitable[dict[str, int]]]

_APPLIERS: dict[str, Applier] = {}


def register(kind: str, fn: Applier) -> Applier:
    """Register ``fn`` as the applier for packet ``kind`` (replaces an earlier registration)."""
    _APPLIERS[kind] = fn
    return fn


def get(kind: str) -> Applier | None:
    return _APPLIERS.get(kind)


async def apply(conn: asyncpg.Connection, packet: PacketRecord, output: BaseModel) -> dict[str, int]:
    """Apply a validated output. Raises KeyError when no applier is registered for the kind."""
    fn = _APPLIERS.get(packet.kind)
    if fn is None:
        raise KeyError(f"no applier registered for packet kind {packet.kind!r}")
    return await fn(conn, packet, output)
