"""Recall engine entry points (spec 11, 6.3, 6.4, 12.5). Owner: recall.

Every function takes a connection opened with ``memsvc.db.user_tx`` for the caller, so RLS bounds
what is visible; the embedder comes from ``memsvc.embed.tei.get_tei()``. Space ids in requests are
resolved with ``memsvc.auth.resolve_read_spaces`` (403 on a foreign space).
"""

from __future__ import annotations

from uuid import UUID

import asyncpg

from memsvc.auth.identity import Identity
from memsvc.schemas.memories import (
    ByFileQuery,
    ExplainResponse,
    GetResponse,
    SearchQuery,
    SearchResponse,
    TimelineQuery,
    TimelineResponse,
)
from memsvc.schemas.recall import RecallRequest, RecallResponse


async def recall(conn: asyncpg.Connection, identity: Identity, req: RecallRequest) -> RecallResponse:
    """Push-mode recall: per-tier BM25 (or tsvector fallback) + vector candidates, RRF, optional
    rerank, final score, MMR, budget fill, markdown render; writes ``retrieval_log``. Spec 11.1-11.5."""
    raise NotImplementedError


async def search(conn: asyncpg.Connection, identity: Identity, query: SearchQuery) -> SearchResponse:
    """Index rows for ``GET /v1/search`` and ``memory_search`` (spec 6.4, 7)."""
    raise NotImplementedError


async def timeline(conn: asyncpg.Connection, identity: Identity, query: TimelineQuery) -> TimelineResponse:
    """Observations and summaries around an anchor in time order, same scope (spec 6.4)."""
    raise NotImplementedError


async def get_memories(conn: asyncpg.Connection, identity: Identity, ids: list[UUID]) -> GetResponse:
    """Full bodies for <= 20 ids across tiers; writes an ``access_log`` row (spec 6.4)."""
    raise NotImplementedError


async def explain(conn: asyncpg.Connection, identity: Identity, memory_id: UUID) -> ExplainResponse:
    """Provenance chain: row, evidence observations with session/device/event seqs, audit (spec 12.5)."""
    raise NotImplementedError


async def by_file(conn: asyncpg.Connection, identity: Identity, query: ByFileQuery) -> SearchResponse:
    """Recent observations touching a file path (``GET /v1/observations/by-file``, spec 6.4)."""
    raise NotImplementedError
