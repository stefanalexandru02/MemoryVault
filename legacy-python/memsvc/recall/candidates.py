"""Candidate SQL per tier (spec 11.2): keyword leg, vector leg, no-query leg, hydration.

Keyword leg: ParadeDB BM25 (``search_text @@@ q`` / ``paradedb.score``) when ``pg_search`` is installed,
otherwise the generated ``search_tsv`` with ``websearch_to_tsquery('simple', ...)`` and ``ts_rank_cd``.
Query words are OR-ed (``a or b``) so a long natural-language query still matches rows that share
only some words; ranking rewards rows that match more of them. ``task_state.entities`` terms are
added with half weight.

Vector leg: best part per owner (``DISTINCT ON (owner_id)`` ordered by distance) over ``embeddings``
for the configured model, with the recency windows of 11.2 (observations 90 d, summaries 180 d)
when ``windows=True`` (recall), none for search.

Fusion happens in Python (``scoring.rrf_fuse``); it is the same RRF the spec writes in SQL.
Every query runs on the caller's ``user_tx`` connection, so RLS bounds what is visible.
"""

from __future__ import annotations

import re
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import datetime
from typing import Any
from uuid import UUID

import asyncpg

from memsvc.db import caps
from memsvc.schemas.common import Scope

KEYWORD_LIMIT = 40
VECTOR_LIMIT = 60
VECTOR_PART_FANOUT = 4  # parts fetched per wanted owner before DISTINCT ON

STOPWORDS = frozenset(
    "a an and are as at be by for from has have how i in is it its of on or that the this to was were what when "
    "where which who why will with after before into about over then than there these those we you my our".split()
)
_WORD = re.compile(r"\w+", re.UNICODE)


def query_terms(text: str | None) -> list[str]:
    """Lowercased distinct words of a query, stopwords and one-character tokens removed, in order."""
    if not text:
        return []
    out: list[str] = []
    for w in _WORD.findall(text.lower()):
        if len(w) > 1 and w not in STOPWORDS and w not in out:
            out.append(w)
    return out


@dataclass(frozen=True)
class TierSpec:
    table: str
    type_col: str  # column matched by the ``types`` filter (and shown as IndexRow.type)
    time_col: str  # creation time column
    has_sensitivity: bool
    live: str  # predicate for rows that may be shown at all
    vector_window_days: int | None  # recall recency window on the vector leg
    recent_order: str  # ORDER BY for the no-query leg


TIERS: dict[str, TierSpec] = {
    "observations": TierSpec(
        "observations",
        "type",
        "created_at",
        True,
        "t.archived_at IS NULL AND t.superseded_by IS NULL",
        90,
        "t.created_at DESC, t.id DESC",
    ),
    "summaries": TierSpec("summaries", "grain", "created_at", False, "true", 180, "t.created_at DESC, t.id DESC"),
    "facts": TierSpec(
        "facts",
        "predicate",
        "first_seen_at",
        True,
        "t.status = 'active' AND (t.valid_to IS NULL OR t.valid_to > now())",
        None,
        "t.pinned DESC, t.importance * t.confidence DESC, t.last_confirmed_at DESC, t.id DESC",
    ),
    "procedures": TierSpec(
        "procedures",
        "task_class",
        "created_at",
        False,
        "t.status = 'active'",
        None,
        "t.last_used_at DESC NULLS LAST, t.success_count DESC, t.created_at DESC, t.id DESC",
    ),
}


@dataclass
class Filters:
    spaces: list[UUID]
    scopes: list[Scope] | None  # None = every scope in the spaces
    exclude_sensitivity: Sequence[str] = ("secret",)
    types: list[str] | None = None
    since: datetime | None = None
    until: datetime | None = None
    files: list[str] | None = None
    summary_grains: list[str] | None = None
    exclude_subagent: bool = False  # SessionStart index hides subagent observations (8.3)
    pinned_only: bool = False  # facts


class Params:
    """Positional parameter collector: ``p(value, "uuid[]")`` -> ``"$3::uuid[]"``."""

    def __init__(self) -> None:
        self.values: list[Any] = []

    def __call__(self, value: Any, cast: str | None = None) -> str:
        self.values.append(value)
        ref = f"${len(self.values)}"
        return f"{ref}::{cast}" if cast else ref


def where_sql(spec: TierSpec, f: Filters, p: Params) -> str:
    parts = [f"t.space_id = ANY({p(f.spaces, 'uuid[]')})", spec.live]
    if f.scopes is not None:
        kinds = p([s.kind for s in f.scopes], "text[]")
        keys = p([s.key for s in f.scopes], "text[]")
        parts.append(f"(t.scope_kind, t.scope_key) IN (SELECT k::scope_kind, v FROM unnest({kinds}, {keys}) AS u(k, v))")
    if spec.has_sensitivity and f.exclude_sensitivity:
        parts.append(f"t.sensitivity <> ALL({p(list(f.exclude_sensitivity), 'text[]')})")
    if f.types:
        parts.append(f"t.{spec.type_col} = ANY({p(f.types, 'text[]')})")
    if f.since is not None:
        parts.append(f"t.{spec.time_col} >= {p(f.since, 'timestamptz')}")
    if f.until is not None:
        parts.append(f"t.{spec.time_col} < {p(f.until, 'timestamptz')}")
    if f.files:
        parts.append(f"t.files && {p(f.files, 'text[]')}" if spec.table == "observations" else "false")
    if f.summary_grains is not None and spec.table == "summaries":
        parts.append(f"t.grain = ANY({p(f.summary_grains, 'text[]')})")
    if f.exclude_subagent and spec.table == "observations":
        parts.append("NOT EXISTS (SELECT 1 FROM unnest(t.entities) AS en(x) WHERE en.x LIKE 'subagent:%')")
    if f.pinned_only and spec.table == "facts":
        parts.append("t.pinned")
    return " AND ".join(parts)


async def keyword_leg(
    conn: asyncpg.Connection,
    tier: str,
    f: Filters,
    terms: list[str],
    entity_terms: list[str] | None = None,
    limit: int = KEYWORD_LIMIT,
) -> list[tuple[UUID, float]]:
    """Ranked ``(id, score)`` from BM25 (pg_search) or the tsvector fallback."""
    entity_terms = [e for e in (entity_terms or []) if e not in terms]
    if not terms:
        terms, entity_terms = entity_terms, []
    if not terms:
        return []
    spec = TIERS[tier]
    p = Params()
    where = where_sql(spec, f, p)
    if caps(conn).pg_search:
        qs = " OR ".join(terms + [f"{e}^0.5" for e in entity_terms])
        sql = (
            f"SELECT t.id, paradedb.score(t.id) AS s FROM {spec.table} t "
            f"WHERE {where} AND t.search_text @@@ {p(qs, 'text')} ORDER BY s DESC, t.id LIMIT {int(limit)}"
        )
    else:
        main = p(" or ".join(terms), "text")
        if entity_terms:
            ent = p(" or ".join(entity_terms), "text")
            q = f"(SELECT websearch_to_tsquery('simple', {main}) AS m, websearch_to_tsquery('simple', {ent}) AS e) q"
            rank = "ts_rank_cd(t.search_tsv, q.m) + 0.5 * ts_rank_cd(t.search_tsv, q.e)"
            match = "t.search_tsv @@ (q.m || q.e)"
        else:
            q = f"(SELECT websearch_to_tsquery('simple', {main}) AS m) q"
            rank = "ts_rank_cd(t.search_tsv, q.m)"
            match = "t.search_tsv @@ q.m"
        sql = (
            f"SELECT t.id, {rank} AS s FROM {spec.table} t, {q} "
            f"WHERE {where} AND {match} ORDER BY s DESC, t.id LIMIT {int(limit)}"
        )
    rows = await conn.fetch(sql, *p.values)
    return [(r["id"], float(r["s"])) for r in rows]


async def vector_leg(
    conn: asyncpg.Connection,
    tier: str,
    f: Filters,
    qvec: list[float],
    model: str,
    *,
    windows: bool = True,
    limit: int = VECTOR_LIMIT,
) -> list[tuple[UUID, float]]:
    """Ranked ``(id, cosine)``, best part per owner."""
    spec = TIERS[tier]
    p = Params()
    where = where_sql(spec, f, p)
    vec = p(qvec, "vector")
    window = ""
    if windows and spec.vector_window_days is not None:
        window = f" AND t.{spec.time_col} > now() - interval '{int(spec.vector_window_days)} days'"
    sql = f"""
        SELECT id, 1 - d AS s FROM (
          SELECT DISTINCT ON (pp.id) pp.id, pp.d FROM (
            SELECT e.owner_id AS id, e.embedding <=> {vec} AS d
              FROM embeddings e JOIN {spec.table} t ON t.id = e.owner_id
             WHERE e.owner_table = '{spec.table}' AND e.model = {p(model, "text")}
               AND e.space_id = ANY({p(f.spaces, "uuid[]")}) AND {where}{window}
             ORDER BY d LIMIT {int(limit) * VECTOR_PART_FANOUT}) pp
          ORDER BY pp.id, pp.d) best
        ORDER BY d, id LIMIT {int(limit)}"""
    rows = await conn.fetch(sql, *p.values)
    return [(r["id"], float(r["s"])) for r in rows]


async def recent_leg(conn: asyncpg.Connection, tier: str, f: Filters, limit: int, offset: int = 0) -> list[UUID]:
    """No-query candidates in the tier's natural order (newest / most used first)."""
    spec = TIERS[tier]
    p = Params()
    where = where_sql(spec, f, p)
    sql = f"SELECT t.id FROM {spec.table} t WHERE {where} ORDER BY {spec.recent_order} LIMIT {int(limit)}"
    if offset:
        sql += f" OFFSET {int(offset)}"
    return [r["id"] for r in await conn.fetch(sql, *p.values)]


_HYDRATE = {
    "observations": """
        SELECT t.*, c.device_id
          FROM observations t
          LEFT JOIN sessions s ON s.id = t.session_id
          LEFT JOIN clients c ON c.id = coalesce(s.client_id, t.written_by_client_id)
         WHERE t.id = ANY($1::uuid[])""",
    "summaries": """
        SELECT t.*, c.device_id
          FROM summaries t
          LEFT JOIN sessions s ON s.id = t.session_id
          LEFT JOIN clients c ON c.id = s.client_id
         WHERE t.id = ANY($1::uuid[])""",
    "facts": """
        SELECT t.*,
               en.canonical_name AS subject_name, en.kind AS subject_kind,
               ARRAY(SELECT DISTINCT c.device_id FROM clients c
                      WHERE c.id = ANY(CASE WHEN cardinality(t.learned_on_client_ids) > 0
                                            THEN t.learned_on_client_ids
                                            ELSE ARRAY[t.written_by_client_id] END)
                      ORDER BY 1) AS devices
          FROM facts t JOIN entities en ON en.id = t.subject_id
         WHERE t.id = ANY($1::uuid[])""",
    "procedures": """
        SELECT t.* FROM procedures t WHERE t.id = ANY($1::uuid[])""",
}


async def hydrate(conn: asyncpg.Connection, tier: str, ids: Sequence[UUID]) -> dict[UUID, asyncpg.Record]:
    if not ids:
        return {}
    rows = await conn.fetch(_HYDRATE[tier], list(ids))
    return {r["id"]: r for r in rows}


async def best_part_embeddings(
    conn: asyncpg.Connection, keys: Sequence[tuple[str, UUID]], model: str, qvec: list[float] | None
) -> dict[tuple[str, UUID], list[float]]:
    """One embedding per ``(owner_table, owner_id)``: the part closest to the query (or the first part)."""
    if not keys:
        return {}
    tables = [k[0] for k in keys]
    ids = [k[1] for k in keys]
    if qvec is not None:
        order, args = "e.embedding <=> $4::vector", [model, tables, ids, qvec]
    else:
        order, args = "e.part", [model, tables, ids]
    rows = await conn.fetch(
        f"""SELECT DISTINCT ON (e.owner_table, e.owner_id) e.owner_table, e.owner_id, e.embedding
              FROM embeddings e
             WHERE e.model = $1
               AND (e.owner_table, e.owner_id) IN (SELECT * FROM unnest($2::text[], $3::uuid[]))
             ORDER BY e.owner_table, e.owner_id, {order}""",
        *args,
    )
    return {(r["owner_table"], r["owner_id"]): r["embedding"] for r in rows}
