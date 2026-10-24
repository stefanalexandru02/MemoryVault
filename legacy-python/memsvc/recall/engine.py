"""Recall engine entry points (spec 11, 6.3, 6.4, 12.5). Owner: recall.

Every function takes a connection opened with ``memsvc.db.user_tx`` for the caller, so RLS bounds
what is visible; the embedder comes from ``memsvc.embed.tei.get_tei()``. Space ids in requests are
resolved with ``memsvc.auth.resolve_read_spaces`` (403 on a foreign space).

Pipeline (``build_pack``): per tier, keyword leg + vector leg (or the no-query leg) -> RRF ->
hydrate -> optional cross-tier rerank of the fused top 40 (250 ms budget, falls back to RRF) ->
final score (11.4) -> MMR within the tier -> greedy budget fill per slot (11.5) -> render.
"""

from __future__ import annotations

import asyncio
import logging
import time
from collections.abc import Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any
from uuid import UUID

import asyncpg

from memsvc.auth.identity import Identity
from memsvc.auth.spaces import resolve_read_spaces
from memsvc.config import get_settings
from memsvc.db import tune_vector_search
from memsvc.embed.tei import TEIError, get_tei
from memsvc.errors import ApiError
from memsvc.ids import uuid7
from memsvc.recall import render
from memsvc.recall.candidates import (
    TIERS,
    Filters,
    best_part_embeddings,
    hydrate,
    keyword_leg,
    query_terms,
    recent_leg,
    vector_leg,
)
from memsvc.recall.scoring import (
    INTENT_BONUS,
    TYPE_BOOST,
    Candidate,
    age_days,
    budget_fill,
    final_score,
    half_life_for,
    mmr_order,
    recency,
    rrf_fuse,
    sort_key,
)
from memsvc.schemas.common import Scope, estimate_tokens
from memsvc.schemas.memories import (
    AuditEntry,
    ByFileQuery,
    EntityRef,
    EvidenceObservation,
    ExplainResponse,
    FactFull,
    GetResponse,
    IndexRow,
    ObservationFull,
    ProcedureFull,
    Provenance,
    SearchQuery,
    SearchResponse,
    SessionInfo,
    SummaryFull,
    TimelineQuery,
    TimelineResponse,
)
from memsvc.schemas.recall import (
    Budgets,
    FactItem,
    ObservationItem,
    ProcedureItem,
    ProfileSlot,
    RecallRequest,
    RecallResponse,
    RecallSlots,
    SummaryItem,
    TokenUsage,
    WorkStateList,
)

log = logging.getLogger(__name__)

TIER_ORDER = ("facts", "procedures", "observations", "summaries")
RECALL_SUMMARY_GRAINS = ["session", "day", "week"]
RERANK_TOP = 40
NO_QUERY_LIMIT = 40
CONTEXT_OBS_ROWS = 30
PINNED_LIMIT = 20
MAX_LOGGED_CANDIDATES = 200
MAX_GET = 20

# ---------- scopes ----------


def validate_scopes(scopes: Sequence[Scope]) -> list[Scope]:
    out: list[Scope] = []
    for s in scopes:
        if s.kind == "global" and s.key != "":
            raise ApiError(400, "invalid_scope", "the global scope has an empty key")
        if s.kind != "global" and not s.key:
            raise ApiError(400, "invalid_scope", f"scope {s.kind} needs a key")
        if s not in out:
            out.append(s)
    return out


async def default_scopes(conn: asyncpg.Connection, identity: Identity) -> list[Scope]:
    """global + the caller's current project (clients.current_project_key) + the caller's device."""
    scopes = [Scope(kind="global", key="")]
    project = await conn.fetchval("SELECT current_project_key FROM clients WHERE id = $1", identity.client_id)
    if project:
        scopes.append(Scope(kind="project", key=project))
    if identity.device_id:
        scopes.append(Scope(kind="device", key=identity.device_id))
    return scopes


def _scope(row: Any) -> Scope:
    return Scope(kind=row["scope_kind"], key=row["scope_key"])


# ---------- the pack ----------


@dataclass
class PackOptions:
    query: str | None = None
    entities: list[str] = field(default_factory=list)
    intent: str | None = None
    budgets: Budgets = field(default_factory=Budgets)
    expand: int = 0
    rerank: bool = False
    exclude_sensitivity: Sequence[str] = ("secret",)
    context: bool = False  # SessionStart: no query, pinned facts only, subagent obs hidden, <= 30 index rows


@dataclass
class Pack:
    slots: RecallSlots
    by_slot: dict[str, int]
    candidates: list[Candidate]
    selected: list[dict[str, Any]]
    reranked: bool


def _rerank_text(c: Candidate) -> str:
    r = c.row
    if c.tier == "observations":
        return " ".join(x for x in (r["title"], r["subtitle"], r["narrative"]) if x)
    if c.tier == "summaries":
        return " ".join(x for x in (r["request"], r["learned"], r["completed"], r["next_steps"]) if x)
    if c.tier == "facts":
        return f"{r['subject_name']} {r['predicate']} {r['value_text']}"
    return f"{r['name']} {r['trigger_text']}"


def _score(c: Candidate, now: datetime, intent: str | None) -> float:
    r = c.row
    if c.tier == "observations":
        rec = recency(age_days(r["created_at"], now), half_life_for("observations", r["type"]))
        return final_score(
            c.base, importance=float(r["importance"]), recency_value=rec, type_boost=TYPE_BOOST.get(r["type"], 1.0)
        )
    if c.tier == "summaries":
        return final_score(c.base, recency_value=recency(age_days(r["created_at"], now), half_life_for("summaries")))
    if c.tier == "facts":
        return final_score(c.base, importance=float(r["importance"]), confidence=float(r["confidence"]))
    bonus = INTENT_BONUS if intent and r["task_class"] == intent else 0.0
    return final_score(c.base, bonus=bonus)


async def _embed_query(query: str) -> list[float] | None:
    try:
        return (await get_tei().embed([query]))[0]
    except (TEIError, asyncio.TimeoutError) as e:
        log.warning("query embedding failed, keyword leg only: %s", e)
        return None


async def _rerank(query: str, cands: list[Candidate]) -> bool:
    """Rerank the fused top 40 across tiers; the rerank score replaces RRF as base. False on timeout/error."""
    top = sorted(cands, key=lambda c: (-c.rrf, c.tier, str(c.id)))[:RERANK_TOP]
    if not top:
        return False
    timeout = get_settings().tei_rerank_timeout_s
    try:
        scores = await asyncio.wait_for(get_tei().rerank(query, [_rerank_text(c) for c in top]), timeout)
    except (TEIError, asyncio.TimeoutError) as e:
        log.info("rerank skipped (%s); using RRF", type(e).__name__)
        return False
    if len(scores) != len(top):
        return False
    for c, s in zip(top, scores, strict=True):
        c.rerank = float(s)
    return True


async def _tier_candidates(
    conn: asyncpg.Connection, tier: str, f: Filters, terms: list[str], ent: list[str], qvec: list[float] | None,
    model: str, has_query: bool, limit: int,
) -> dict[UUID, Candidate]:  # fmt: skip
    if has_query:
        kw = await keyword_leg(conn, tier, f, terms, ent) if (terms or ent) else []
        vec = await vector_leg(conn, tier, f, qvec, model) if qvec is not None else []
        return rrf_fuse(tier, kw, vec)
    ids = await recent_leg(conn, tier, f, limit)
    return {i: Candidate(i, tier, rrf=1.0) for i in ids}


async def build_pack(
    conn: asyncpg.Connection, spaces: list[UUID], scopes: list[Scope], opts: PackOptions
) -> Pack:
    now = datetime.now(UTC)
    tei = get_tei()
    budgets = opts.budgets
    has_query = bool(opts.query and opts.query.strip())
    qvec = await _embed_query(opts.query) if has_query and opts.query else None
    if qvec is not None:
        await tune_vector_search(conn)
    terms = query_terms(opts.query)
    ent = query_terms(" ".join(opts.entities))

    def filters(**kw: Any) -> Filters:
        return Filters(
            spaces=spaces,
            scopes=scopes,
            exclude_sensitivity=opts.exclude_sensitivity,
            summary_grains=RECALL_SUMMARY_GRAINS,
            exclude_subagent=opts.context,
            **kw,
        )

    budget_of = {
        "facts": budgets.facts,
        "procedures": budgets.procedures,
        "observations": budgets.observations,
        "summaries": budgets.summaries,
    }
    by_tier: dict[str, dict[UUID, Candidate]] = {}
    for tier in TIER_ORDER:
        if budget_of[tier] <= 0 or (tier == "facts" and opts.context):
            by_tier[tier] = {}
            continue
        limit = max(NO_QUERY_LIMIT, CONTEXT_OBS_ROWS * 2) if tier == "observations" else NO_QUERY_LIMIT
        by_tier[tier] = await _tier_candidates(
            conn, tier, filters(), terms, ent, qvec, tei.model, has_query, limit
        )

    # Pinned facts are added regardless of score (11.2).
    pinned_ids: list[UUID] = []
    if budgets.facts > 0:
        pinned_ids = await recent_leg(conn, "facts", filters(pinned_only=True), PINNED_LIMIT)
        for pid in pinned_ids:
            by_tier["facts"].setdefault(pid, Candidate(pid, "facts"))

    # Profile slot: profiles rows for the scopes; without any, a fallback rendering of the top facts.
    profile, profile_fact_ids, profile_cost = await _profile_slot(conn, spaces, scopes, opts, now)

    # Hydrate.
    for tier, cands in by_tier.items():
        rows = await hydrate(conn, tier, list(cands))
        for cid in list(cands):
            if cid in rows:
                cands[cid].row = rows[cid]
            else:
                del cands[cid]

    all_cands = [c for cands in by_tier.values() for c in cands.values()]
    reranked = False
    if opts.rerank and has_query and opts.query:
        reranked = await _rerank(opts.query, [c for c in all_cands if c.rrf > 0])
    for c in all_cands:
        c.score = _score(c, now, opts.intent)

    embs = await best_part_embeddings(conn, [(c.tier, c.id) for c in all_cands], tei.model, qvec)
    for c in all_cands:
        c.embedding = embs.get((c.tier, c.id))

    slots = RecallSlots(profile=profile)
    by_slot = {"profile": profile_cost}
    selected: list[dict[str, Any]] = []

    # facts: pinned first, then by MMR order; facts already in the fallback profile are skipped.
    fact_cands = [c for c in by_tier["facts"].values() if c.id not in profile_fact_ids]
    pinned_set = set(pinned_ids)
    pinned = sorted((c for c in fact_cands if c.id in pinned_set), key=sort_key)
    ranked = [] if opts.context else mmr_order([c for c in fact_cands if c.id not in pinned_set and c.rrf > 0])
    fact_items = [_fact_item(c) for c in pinned + ranked]
    slots.facts = budget_fill(fact_items, lambda i: estimate_tokens(render.fact_line(i)), budgets.facts)
    by_slot["facts"] = sum(estimate_tokens(render.fact_line(i)) for i in slots.facts)

    proc_items = [_procedure_item(c) for c in mmr_order(list(by_tier["procedures"].values()))]
    slots.procedures = budget_fill(proc_items, lambda i: estimate_tokens(render.procedure_line(i)), budgets.procedures)
    by_slot["procedures"] = sum(estimate_tokens(render.procedure_line(i)) for i in slots.procedures)

    slots.observations, by_slot["observations"] = _fill_observations(
        mmr_order(list(by_tier["observations"].values())), budgets.observations, opts
    )

    sum_items = [_summary_item(c) for c in mmr_order(list(by_tier["summaries"].values()))]
    slots.summaries = budget_fill(sum_items, lambda i: estimate_tokens(render.summary_line(i)), budgets.summaries)
    by_slot["summaries"] = sum(estimate_tokens(render.summary_line(i)) for i in slots.summaries)

    if budgets.work_state > 0:
        lists = await fold_work_state(conn, spaces, scopes)
        slots.work_state = budget_fill(lists, lambda w: w.token_estimate, budgets.work_state)
    by_slot["work_state"] = sum(w.token_estimate for w in slots.work_state)

    for slot_name in ("facts", "procedures", "observations", "summaries"):
        for item in getattr(slots, slot_name):
            selected.append({"id": str(item.id), "slot": slot_name, "citation": item.citation})
    for fid in profile_fact_ids:
        selected.append({"id": str(fid), "slot": "profile"})
    return Pack(slots=slots, by_slot=by_slot, candidates=all_cands, selected=selected, reranked=reranked)


def _fill_observations(
    ordered: list[Candidate], budget: int, opts: PackOptions
) -> tuple[list[ObservationItem], int]:
    out: list[ObservationItem] = []
    used = 0
    expanded = 0
    max_rows = CONTEXT_OBS_ROWS if opts.context else None
    for c in ordered:
        if max_rows is not None and len(out) >= max_rows:
            break
        if expanded < opts.expand:
            item = _observation_item(c, expand=True)
            cost = item.token_estimate or estimate_tokens(render.observation_block(item))
            if used + cost <= budget:
                out.append(item)
                used += cost
                expanded += 1
                continue
        item = _observation_item(c, expand=False)
        cost = estimate_tokens(render.index_line(render.entry_from_observation(item)))
        if used + cost <= budget:
            out.append(item)
            used += cost
    return out, used


async def _profile_slot(
    conn: asyncpg.Connection, spaces: list[UUID], scopes: list[Scope], opts: PackOptions, now: datetime
) -> tuple[ProfileSlot | None, set[UUID], int]:
    budget = opts.budgets.profile
    if budget <= 0:
        return None, set(), 0
    rows = await conn.fetch(
        """SELECT space_id, scope_kind, scope_key, body_md, token_estimate FROM profiles
            WHERE space_id = ANY($1::uuid[])
              AND (scope_kind, scope_key) IN (SELECT k::scope_kind, v FROM unnest($2::text[], $3::text[]) u(k, v))""",
        spaces,
        [s.kind for s in scopes],
        [s.key for s in scopes],
    )
    if rows:
        scope_pos = {s: i for i, s in enumerate(scopes)}
        space_pos = {s: i for i, s in enumerate(spaces)}
        rows = sorted(rows, key=lambda r: (scope_pos.get(_scope(r), 99), space_pos.get(r["space_id"], 99)))
        lines: list[str] = []
        used = 0
        for r in rows:
            for line in r["body_md"].strip().splitlines():
                cost = estimate_tokens(line + "\n")
                if used + cost > budget:
                    break
                lines.append(line)
                used += cost
        body = "\n".join(lines)
        used_scopes = list(dict.fromkeys(_scope(r) for r in rows))
        return ProfileSlot(body_md=body, token_estimate=estimate_tokens(body), scopes=used_scopes), set(), used
    # Fallback: no profile rendered yet for these scopes -> top active facts.
    f = Filters(spaces=spaces, scopes=scopes, exclude_sensitivity=opts.exclude_sensitivity)
    ids = await recent_leg(conn, "facts", f, PINNED_LIMIT)
    hyd = await hydrate(conn, "facts", ids)
    items = [_fact_item(Candidate(i, "facts", row=hyd[i])) for i in ids if i in hyd]
    chosen = budget_fill(items, lambda i: estimate_tokens(render.fact_line(i) + "\n"), budget)
    if not chosen:
        return None, set(), 0
    body = "\n".join(render.fact_line(i) for i in chosen)
    used_scopes = list(dict.fromkeys(i.scope for i in chosen))
    return ProfileSlot(body_md=body, token_estimate=estimate_tokens(body), scopes=used_scopes), {
        i.id for i in chosen
    }, estimate_tokens(body)


async def fold_work_state(conn: asyncpg.Connection, spaces: list[UUID], scopes: list[Scope]) -> list[WorkStateList]:
    """Folded lists (last write per key wins; a null value removes the key), newest list first."""
    rows = await conn.fetch(
        """SELECT space_id, scope_kind, scope_key, list_name, fields, created_at FROM work_state
            WHERE space_id = ANY($1::uuid[])
              AND (scope_kind, scope_key) IN (SELECT k::scope_kind, v FROM unnest($2::text[], $3::text[]) u(k, v))
            ORDER BY created_at, id""",
        spaces,
        [s.kind for s in scopes],
        [s.key for s in scopes],
    )
    folded: dict[tuple[Scope, str], WorkStateList] = {}
    for r in rows:
        key = (_scope(r), r["list_name"])
        w = folded.setdefault(key, WorkStateList(scope=key[0], list_name=key[1], fields={}))
        for k, v in (r["fields"] or {}).items():
            if v is None:
                w.fields.pop(k, None)
            else:
                w.fields[k] = v
        w.updated_at = r["created_at"]
    out = [w for w in folded.values() if w.fields]
    for w in out:
        w.token_estimate = estimate_tokens(render.work_state_line(w))
    out.sort(key=lambda w: (-(w.updated_at.timestamp() if w.updated_at else 0), w.list_name, w.scope.as_text()))
    return out


# ---------- slot items ----------


def _fact_item(c: Candidate) -> FactItem:
    r = c.row
    devices = [d for d in (r["devices"] or []) if d]
    return FactItem(
        id=r["id"],
        space_id=r["space_id"],
        scope=_scope(r),
        score=round(c.score, 6),
        subject=r["subject_name"],
        subject_kind=r["subject_kind"],
        predicate=r["predicate"],
        value=r["value"],
        value_text=r["value_text"],
        confidence=float(r["confidence"]),
        importance=float(r["importance"]),
        pinned=bool(r["pinned"]),
        learned_on=devices,
        first_seen_at=r["first_seen_at"],
        last_confirmed_at=r["last_confirmed_at"],
        citation=render.fact_tag(r["id"], float(r["confidence"]), devices, r["first_seen_at"]),
        token_estimate=estimate_tokens(r["value_text"]),
    )


def _procedure_item(c: Candidate) -> ProcedureItem:
    r = c.row
    return ProcedureItem(
        id=r["id"],
        space_id=r["space_id"],
        scope=_scope(r),
        score=round(c.score, 6),
        token_estimate=r["token_estimate"] or estimate_tokens(r["body_md"]),
        citation=render.procedure_tag(r["id"], r["version"], r["created_at"]),
        name=r["name"],
        task_class=r["task_class"],
        trigger_text=r["trigger_text"],
        body_md=r["body_md"],
        version=r["version"],
        success_count=r["success_count"],
        last_used_at=r["last_used_at"],
    )


def _observation_item(c: Candidate, expand: bool) -> ObservationItem:
    r = c.row
    return ObservationItem(
        id=r["id"],
        space_id=r["space_id"],
        scope=_scope(r),
        score=round(c.score, 6),
        token_estimate=r["token_estimate"]
        or estimate_tokens(" ".join([r["title"], r["subtitle"] or "", r["narrative"] or "", *r["facts"]])),
        citation=render.obs_tag(r["id"], r["device_id"], r["created_at"]),
        type=r["type"],
        title=r["title"],
        subtitle=r["subtitle"],
        narrative=r["narrative"] if expand else None,
        facts=list(r["facts"]) if expand else [],
        files=list(r["files"]),
        importance=float(r["importance"]),
        created_at=r["created_at"],
        session_id=r["session_id"],
        device_id=r["device_id"],
        expanded=expand,
    )


def _summary_item(c: Candidate) -> SummaryItem:
    r = c.row
    return SummaryItem(
        id=r["id"],
        space_id=r["space_id"],
        scope=_scope(r),
        score=round(c.score, 6),
        token_estimate=r["token_estimate"],
        citation=render.summary_tag(r["id"], r["device_id"], r["created_at"]),
        grain=r["grain"],
        request=r["request"],
        learned=r["learned"],
        completed=r["completed"],
        next_steps=r["next_steps"],
        created_at=r["created_at"],
        session_id=r["session_id"],
        device_id=r["device_id"],
    )


# ---------- POST /v1/recall ----------


async def log_retrieval(
    conn: asyncpg.Connection,
    identity: Identity,
    *,
    mode: str,
    query: str | None,
    spaces: list[UUID],
    scopes: list[Scope],
    budgets: Budgets | None,
    pack: Pack,
    latency_ms: int,
) -> UUID:
    rid = uuid7()
    cands = sorted(pack.candidates, key=lambda c: (c.tier, *sort_key(c)))[:MAX_LOGGED_CANDIDATES]
    await conn.execute(
        """INSERT INTO retrieval_log (id, space_id, client_id, mode, query, scopes, budgets, candidates, selected,
                                      latency_ms, reranked)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)""",
        rid,
        spaces[0],
        identity.client_id,
        mode,
        query,
        {"space_ids": [str(s) for s in spaces], "scopes": [s.as_text() for s in scopes]},
        budgets.model_dump() if budgets else None,
        [c.log() for c in cands],
        pack.selected,
        latency_ms,
        pack.reranked,
    )
    return rid


async def recall(conn: asyncpg.Connection, identity: Identity, req: RecallRequest) -> RecallResponse:
    """Push-mode recall: per-tier BM25 (or tsvector fallback) + vector candidates, RRF, optional
    rerank, final score, MMR, budget fill, markdown render; writes ``retrieval_log``. Spec 11.1-11.5."""
    started = time.perf_counter()
    spaces = await resolve_read_spaces(conn, identity, req.space_ids)
    if not spaces:
        raise ApiError(403, "space_forbidden", "the caller has no readable space")
    scopes = validate_scopes(req.scopes) if req.scopes else await default_scopes(conn, identity)
    ts = req.task_state
    opts = PackOptions(
        query=req.query,
        entities=list(ts.entities) if ts else [],
        intent=ts.intent if ts else None,
        budgets=req.budgets,
        expand=req.expand_top_observations,
        rerank=req.rerank,
        exclude_sensitivity=list(req.exclude_sensitivity),
    )
    pack = await build_pack(conn, spaces, scopes, opts)
    rendered = None if req.format == "json" else render.recall_markdown(pack.slots, opts.expand)
    latency_ms = int((time.perf_counter() - started) * 1000)
    rid = await log_retrieval(
        conn,
        identity,
        mode="recall",
        query=req.query,
        spaces=spaces,
        scopes=scopes,
        budgets=req.budgets,
        pack=pack,
        latency_ms=latency_ms,
    )
    return RecallResponse(
        retrieval_id=rid,
        slots=pack.slots,
        rendered_markdown=rendered,
        tokens=TokenUsage(total=sum(pack.by_slot.values()), by_slot=pack.by_slot),
        latency_ms=latency_ms,
        reranked=pack.reranked,
    )


async def feedback(
    conn: asyncpg.Connection, identity: Identity, retrieval_id: UUID, used_ids: list[UUID], helpful: bool | None,
    notes: str | None,
) -> bool:  # fmt: skip
    """Record feedback on a retrieval and bump ``access_count``/``last_accessed_at`` of used facts
    (``last_used_at`` of used procedures). Only the client that made the retrieval may report on it."""
    row = await conn.fetchrow(
        """UPDATE retrieval_log SET feedback = $3 WHERE id = $1 AND client_id = $2 RETURNING id""",
        retrieval_id,
        identity.client_id,
        {
            "used_ids": [str(u) for u in used_ids],
            "helpful": helpful,
            "notes": notes,
            "at": datetime.now(UTC).isoformat(),
        },
    )
    if row is None:
        raise ApiError(404, "not_found", "retrieval not found")
    if used_ids:
        await conn.execute(
            """UPDATE facts SET access_count = access_count + 1, last_accessed_at = now()
                WHERE id = ANY($1::uuid[])""",
            used_ids,
        )
        await conn.execute("UPDATE procedures SET last_used_at = now() WHERE id = ANY($1::uuid[])", used_ids)
    return True


# ---------- index rows (search, timeline, by-file) ----------


def _index_row(tier: str, r: Any, score: float | None = None, anchor: bool = False) -> IndexRow:
    if tier == "observations":
        title, type_, created = r["title"], r["type"], r["created_at"]
        tokens = r["token_estimate"] or estimate_tokens(r["title"] + " " + (r["narrative"] or ""))
    elif tier == "summaries":
        title = r["request"] or r["learned"] or r["completed"] or r["next_steps"] or f"{r['grain']} summary"
        type_, created = r["grain"], r["created_at"]
        tokens = r["token_estimate"] or estimate_tokens(
            " ".join(x for x in (r["request"], r["learned"], r["completed"], r["next_steps"]) if x)
        )
    elif tier == "facts":
        title, type_, created = f"{r['predicate']}: {r['value_text']}", r["predicate"], r["first_seen_at"]
        tokens = estimate_tokens(r["value_text"])
    else:
        title, type_, created = r["name"], r["task_class"], r["created_at"]
        tokens = r["token_estimate"] or estimate_tokens(r["body_md"])
    return IndexRow(
        id=r["id"],
        table=tier,  # type: ignore[arg-type]
        type=type_,
        title=render.one_line(title)[:200],
        created_at=created,
        scope=_scope(r),
        token_estimate=tokens,
        score=None if score is None else round(score, 6),
        is_anchor=anchor,
    )


async def search(conn: asyncpg.Connection, identity: Identity, query: SearchQuery) -> SearchResponse:
    """Index rows for ``GET /v1/search`` and ``memory_search`` (spec 6.4, 7)."""
    spaces = await resolve_read_spaces(conn, identity, query.space_ids)
    scopes = validate_scopes(query.scopes) if query.scopes else None
    tables = list(query.tables or ["observations", "summaries"])
    if query.files:
        tables = [t for t in tables if t == "observations"]
    want = query.offset + query.limit
    f = Filters(
        spaces=spaces,
        scopes=scopes,
        exclude_sensitivity=("secret",),
        types=query.types,
        since=query.since,
        until=query.until,
        files=query.files,
    )
    q = (query.q or "").strip()
    now = datetime.now(UTC)
    rows: list[tuple[tuple[Any, ...], IndexRow]] = []
    if not spaces or not tables:
        return SearchResponse(rows=[], limit=query.limit, offset=query.offset)
    if q:
        terms = query_terms(q)
        qvec = await _embed_query(q)
        if qvec is not None:
            await tune_vector_search(conn)
        model = get_tei().model
        for tier in tables:
            kw = await keyword_leg(conn, tier, f, terms, limit=max(40, want)) if terms else []
            vec = await vector_leg(conn, tier, f, qvec, model, windows=False, limit=max(60, want)) if qvec else []
            cands = rrf_fuse(tier, kw, vec)
            hyd = await hydrate(conn, tier, list(cands))
            for c in cands.values():
                if c.id not in hyd:
                    continue
                c.row = hyd[c.id]
                c.score = _score(c, now, None)
                rows.append(((-c.score, str(c.id)), _index_row(tier, c.row, c.score)))
    else:
        for tier in tables:
            ids = await recent_leg(conn, tier, f, want)
            hyd = await hydrate(conn, tier, ids)
            for i in ids:
                if i in hyd:
                    row = _index_row(tier, hyd[i])
                    rows.append(((-row.created_at.timestamp(), str(i)), row))
    rows.sort(key=lambda x: x[0])
    page = [r for _, r in rows[query.offset : query.offset + query.limit]]
    return SearchResponse(rows=page, limit=query.limit, offset=query.offset)


async def timeline(conn: asyncpg.Connection, identity: Identity, query: TimelineQuery) -> TimelineResponse:
    """Observations and summaries around an anchor in time order, same scope (spec 6.4)."""
    anchor = await conn.fetchrow(
        """SELECT 'observations' AS tbl, space_id, scope_kind, scope_key, created_at FROM observations WHERE id = $1
           UNION ALL
           SELECT 'summaries', space_id, scope_kind, scope_key, created_at FROM summaries WHERE id = $1""",
        query.anchor_id,
    )
    if anchor is None:
        raise ApiError(404, "not_found", "anchor not found")
    obs_live = TIERS["observations"].live
    items_sql = f"""
        WITH items AS (
          SELECT 'observations' AS tbl, t.id, t.created_at FROM observations t
           WHERE t.space_id = $1 AND t.scope_kind = $2::scope_kind AND t.scope_key = $3
             AND ((({obs_live}) AND t.sensitivity <> 'secret') OR t.id = $5)
          UNION ALL
          SELECT 'summaries', t.id, t.created_at FROM summaries t
           WHERE t.space_id = $1 AND t.scope_kind = $2::scope_kind AND t.scope_key = $3)
        (SELECT * FROM items WHERE (created_at, id) < ($4, $5) ORDER BY created_at DESC, id DESC LIMIT $6)
        UNION ALL (SELECT * FROM items WHERE id = $5)
        UNION ALL
        (SELECT * FROM items WHERE (created_at, id) > ($4, $5) ORDER BY created_at, id LIMIT $7)"""
    refs = await conn.fetch(
        items_sql,
        anchor["space_id"],
        anchor["scope_kind"],
        anchor["scope_key"],
        anchor["created_at"],
        query.anchor_id,
        query.before,
        query.after,
    )
    by_table: dict[str, list[UUID]] = {}
    for r in refs:
        by_table.setdefault(r["tbl"], []).append(r["id"])
    hyd = {t: await hydrate(conn, t, ids) for t, ids in by_table.items()}
    out = [
        _index_row(r["tbl"], hyd[r["tbl"]][r["id"]], anchor=r["id"] == query.anchor_id)
        for r in refs
        if r["id"] in hyd[r["tbl"]]
    ]
    out.sort(key=lambda x: (x.created_at, str(x.id)))
    return TimelineResponse(anchor_id=query.anchor_id, rows=out)


async def by_file(conn: asyncpg.Connection, identity: Identity, query: ByFileQuery) -> SearchResponse:
    """Recent observations touching a file path (``GET /v1/observations/by-file``, spec 6.4).

    A stored path matches when it equals the given path or one is a ``/``-suffix of the other, so
    absolute and repo-relative paths meet."""
    spaces = await resolve_read_spaces(conn, identity, None)
    args: list[Any] = [spaces, query.path, query.limit]
    project_sql = ""
    if query.project_key:
        args.append(query.project_key)
        project_sql = """ AND ((t.scope_kind = 'project' AND t.scope_key = $4)
                           OR t.session_id IN (SELECT id FROM sessions WHERE project_key = $4))"""
    ids = await conn.fetch(
        f"""SELECT t.id FROM observations t
             WHERE t.space_id = ANY($1::uuid[]) AND {TIERS["observations"].live} AND t.sensitivity <> 'secret'
               AND EXISTS (SELECT 1 FROM unnest(t.files) AS fl(p)
                            WHERE fl.p = $2 OR right($2, length(fl.p) + 1) = '/' || fl.p
                               OR right(fl.p, length($2) + 1) = '/' || $2){project_sql}
             ORDER BY t.created_at DESC, t.id DESC LIMIT $3""",
        *args,
    )
    id_list = [r["id"] for r in ids]
    hyd = await hydrate(conn, "observations", id_list)
    rows = [_index_row("observations", hyd[i]) for i in id_list if i in hyd]
    return SearchResponse(rows=rows, limit=query.limit, offset=0)


# ---------- full bodies ----------


def _pick(model: type, row: Any, **overrides: Any) -> dict[str, Any]:
    data = {k: row[k] for k in model.model_fields if k in row.keys() and k not in overrides}  # type: ignore[attr-defined]
    data.update(overrides)
    return data


async def load_items(
    conn: asyncpg.Connection, ids: Sequence[UUID]
) -> dict[UUID, ObservationFull | SummaryFull | FactFull | ProcedureFull]:
    """Full bodies for ids across the four tiers (whatever RLS lets the caller see)."""
    out: dict[UUID, ObservationFull | SummaryFull | FactFull | ProcedureFull] = {}
    ids = list(ids)
    for r in await conn.fetch("SELECT * FROM observations WHERE id = ANY($1::uuid[])", ids):
        out[r["id"]] = ObservationFull(**_pick(ObservationFull, r, scope=_scope(r)))
    for r in await conn.fetch("SELECT * FROM summaries WHERE id = ANY($1::uuid[])", ids):
        out[r["id"]] = SummaryFull(**_pick(SummaryFull, r, scope=_scope(r)))
    for r in await conn.fetch(
        """SELECT f.*, e.kind AS subject_kind, e.canonical_name AS subject_name
             FROM facts f JOIN entities e ON e.id = f.subject_id WHERE f.id = ANY($1::uuid[])""",
        ids,
    ):
        subject = EntityRef(id=r["subject_id"], kind=r["subject_kind"], canonical_name=r["subject_name"])
        out[r["id"]] = FactFull(**_pick(FactFull, r, scope=_scope(r), subject=subject))
    for r in await conn.fetch("SELECT * FROM procedures WHERE id = ANY($1::uuid[])", ids):
        out[r["id"]] = ProcedureFull(**_pick(ProcedureFull, r, scope=_scope(r)))
    return out


async def log_access(conn: asyncpg.Connection, identity: Identity, action: str, items: Sequence[Any]) -> None:
    """One ``access_log`` row per space touched."""
    per_space: dict[UUID, list[UUID]] = {}
    for item in items:
        per_space.setdefault(item.space_id, []).append(item.id)
    for space_id, target_ids in per_space.items():
        await conn.execute(
            "INSERT INTO access_log (id, space_id, client_id, action, target_ids) VALUES ($1, $2, $3, $4, $5)",
            uuid7(),
            space_id,
            identity.client_id,
            action,
            target_ids,
        )


async def get_memories(conn: asyncpg.Connection, identity: Identity, ids: list[UUID]) -> GetResponse:
    """Full bodies for <= 20 ids across tiers; writes an ``access_log`` row (spec 6.4)."""
    if len(ids) > MAX_GET:
        raise ApiError(400, "invalid_request", f"at most {MAX_GET} ids")
    wanted = list(dict.fromkeys(ids))
    found = await load_items(conn, wanted)
    items = [found[i] for i in wanted if i in found]
    await log_access(conn, identity, "get", items)
    return GetResponse(items=items, missing=[i for i in wanted if i not in found])


async def resolve_refs(conn: asyncpg.Connection, refs: Sequence[str]) -> dict[str, UUID | None]:
    """Map short display refs (the 6-hex ``obs#a1b2c3`` part) or full ids to visible row ids.

    For the MCP ``memory_get`` tool, whose input may come from the SessionStart index. A ref that
    matches no row, or more than one, maps to None."""
    out: dict[str, UUID | None] = {}
    for ref in refs:
        text = ref.strip().lower().split("#")[-1]
        try:
            out[ref] = UUID(text)
            continue
        except ValueError:
            pass
        if not text or len(text) < 4 or any(ch not in "0123456789abcdef" for ch in text):
            out[ref] = None
            continue
        rows = await conn.fetch(
            """SELECT id FROM observations WHERE replace(id::text, '-', '') LIKE '%' || $1
               UNION ALL SELECT id FROM summaries WHERE replace(id::text, '-', '') LIKE '%' || $1
               UNION ALL SELECT id FROM facts WHERE replace(id::text, '-', '') LIKE '%' || $1
               UNION ALL SELECT id FROM procedures WHERE replace(id::text, '-', '') LIKE '%' || $1
               LIMIT 2""",
            text,
        )
        out[ref] = rows[0]["id"] if len(rows) == 1 else None
    return out


# ---------- explain (12.5) ----------


async def _provenance(conn: asyncpg.Connection, session_id: UUID | None, event_ids: Sequence[UUID],
                      extracted_by: dict[str, Any] | None) -> Provenance:  # fmt: skip
    session = None
    if session_id is not None:
        s = await conn.fetchrow(
            """SELECT s.id, s.harness, s.project_key, s.started_at, c.device_id, c.hostname, c.kind AS client_kind
                 FROM sessions s LEFT JOIN clients c ON c.id = s.client_id WHERE s.id = $1""",
            session_id,
        )
        if s is not None:
            session = SessionInfo(**dict(s))
    seqs: list[int] = []
    if event_ids:
        seqs = [
            r["seq"]
            for r in await conn.fetch("SELECT seq FROM events WHERE id = ANY($1::uuid[]) ORDER BY seq", list(event_ids))
        ]
    return Provenance(session=session, event_ids=list(event_ids), event_seqs=seqs, extracted_by=extracted_by or {})


async def explain(conn: asyncpg.Connection, identity: Identity, memory_id: UUID) -> ExplainResponse:
    """Provenance chain: row, evidence observations with session/device/event seqs, audit (spec 12.5)."""
    item = (await load_items(conn, [memory_id])).get(memory_id)
    if item is None:
        raise ApiError(404, "not_found", "memory not found")
    provenance: Provenance | None = None
    evidence: list[EvidenceObservation] = []
    if isinstance(item, ObservationFull):
        provenance = await _provenance(conn, item.session_id, item.source_event_ids, item.extracted_by)
    elif isinstance(item, SummaryFull):
        provenance = await _provenance(conn, item.session_id, [], item.extracted_by)
    else:
        ev_ids = item.evidence_observation_ids if isinstance(item, FactFull) else item.source_observation_ids
        if isinstance(item, FactFull) and item.extracted_by:
            provenance = Provenance(extracted_by=item.extracted_by)
        ev_items = await load_items(conn, ev_ids)
        for oid in ev_ids:
            o = ev_items.get(oid)
            if isinstance(o, ObservationFull):
                evidence.append(
                    EvidenceObservation(
                        id=o.id,
                        type=o.type,
                        title=o.title,
                        created_at=o.created_at,
                        provenance=await _provenance(conn, o.session_id, o.source_event_ids, o.extracted_by),
                    )
                )
    audit_rows = await conn.fetch(
        """SELECT id, actor, action, before, after, reason, model, prompt_version, run_id, created_at
             FROM memory_audit WHERE target_table = $1 AND target_id = $2 ORDER BY created_at, id""",
        item.table,
        memory_id,
    )
    await log_access(conn, identity, "explain", [item])
    return ExplainResponse(
        id=memory_id,
        table=item.table,
        item=item,
        provenance=provenance,
        evidence=evidence,
        audit=[AuditEntry(**dict(r)) for r in audit_rows],
    )
