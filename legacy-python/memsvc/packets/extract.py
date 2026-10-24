"""Applying an ``extract`` result (spec 10.4), one transaction with the packet's ``done`` update. Owner: ingest."""

from __future__ import annotations

import hashlib
import logging
import math
import unicodedata
from datetime import UTC, datetime
from typing import Any
from uuid import UUID

import asyncpg
from pydantic import BaseModel

from memsvc.embed.jobs import enqueue_embed
from memsvc.embed.tei import get_tei
from memsvc.ids import uuid7
from memsvc.invalidate import notify_invalidate
from memsvc.packets import builders
from memsvc.packets.appliers import register
from memsvc.schemas.common import estimate_tokens
from memsvc.schemas.outputs import FactCandidate, ObservedObservation, ObserverV1
from memsvc.schemas.work import PacketRecord

log = logging.getLogger(__name__)

DEDUP_WINDOW = "24 hours"
SAME_VALUE_COSINE = 0.95


# ---------- helpers shared with the summary appliers ----------


def extracted_by(packet: PacketRecord) -> dict[str, Any]:
    meta = packet.result_meta or {}
    return {
        "runner": meta.get("runner"),
        "runner_kind": meta.get("runner_kind"),
        "model": meta.get("model"),
        "prompt": packet.prompt_ref,
        "packet_id": str(packet.id),
    }


def scope_for(hint: str | None, session: asyncpg.Record) -> tuple[str, str]:
    """Scope of a derived row: project by default; ``device`` / ``global`` when the prompt says so (10.4).
    A session without a project key falls back to its device."""
    if hint == "global":
        return "global", ""
    if hint == "device" or not session["project_key"]:
        return "device", session["device_id"]
    return "project", session["project_key"]


async def audit(
    conn: asyncpg.Connection,
    packet: PacketRecord,
    action: str,
    table: str,
    target_id: UUID,
    *,
    before: Any = None,
    after: Any = None,
    reason: str | None = None,
) -> None:
    await conn.execute(
        """INSERT INTO memory_audit (id, space_id, actor, action, target_table, target_id, before, after, reason,
                                    model, prompt_version)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)""",
        uuid7(),
        packet.space_id,
        f"packet:{packet.id}",
        action,
        table,
        target_id,
        before,
        after,
        reason,
        (packet.result_meta or {}).get("model"),
        packet.prompt_ref,
    )


# ---------- observations ----------


def content_hash(obs: ObservedObservation) -> str:
    return hashlib.sha256("|".join([obs.type, obs.title, *obs.facts]).encode()).hexdigest()


def normalize_title(title: str | None) -> str:
    """Lowercase; punctuation, symbols and emoji become spaces; whitespace collapsed (claude-mem's rule)."""
    text = unicodedata.normalize("NFC", title or "").lower()
    kept = "".join(ch if ch.isspace() or unicodedata.category(ch)[0] in "LNM" else " " for ch in text)
    norm = " ".join(kept.split())
    return norm if any(unicodedata.category(ch)[0] in "LN" for ch in norm) else ""


def title_norm_key(title: str | None, scope: tuple[str, str], platform: str, subagent: bool) -> str | None:
    """Key for the near-duplicate check, unique per (scope, platform, agent-ness, normalized title).
    NULL when the title normalizes to nothing."""
    norm = normalize_title(title)
    if not norm:
        return None
    raw = "\x00".join([f"{scope[0]}:{scope[1]}", platform, "subagent" if subagent else "main", norm])
    return hashlib.sha256(raw.encode()).hexdigest()[:32]


async def _insert_observation(
    conn: asyncpg.Connection,
    packet: PacketRecord,
    session: asyncpg.Record,
    obs: ObservedObservation,
    scope: tuple[str, str],
    chash: str,
    norm_key: str | None,
    event_ids: dict[int, UUID],
) -> UUID:
    obs_id = uuid7()
    body = " ".join([obs.title, obs.subtitle or "", obs.narrative or "", *obs.facts])
    await conn.execute(
        """INSERT INTO observations (id, space_id, scope_kind, scope_key, session_id, window_no, type, title, subtitle,
                                    narrative, facts, concepts, entities, files, importance, sensitivity,
                                    source_event_ids, extracted_by, content_hash, title_norm_key, token_estimate)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)""",
        obs_id,
        packet.space_id,
        scope[0],
        scope[1],
        session["id"],
        packet.inputs.get("window_no"),
        obs.type,
        obs.title,
        obs.subtitle,
        obs.narrative,
        obs.facts,
        list(obs.concepts),
        obs.entities,
        obs.files,
        obs.importance,
        obs.sensitivity,
        [event_ids[s] for s in dict.fromkeys(obs.source_event_seqs) if s in event_ids],
        extracted_by(packet),
        chash,
        norm_key,
        estimate_tokens(body),
    )
    await audit(conn, packet, "obs.create", "observations", obs_id, after={"type": obs.type, "title": obs.title})
    await enqueue_embed(conn, "observations", obs_id)
    return obs_id


# ---------- fact candidates ----------


async def resolve_entity(conn: asyncpg.Connection, space_id: UUID, kind: str, name: str) -> UUID:
    """Existing entity of ``kind`` whose canonical name or an alias matches ``name`` (case-insensitive),
    else a new one."""
    name = name.strip()
    found = await conn.fetchval(
        """SELECT id FROM entities
            WHERE space_id = $1 AND kind = $2
              AND (lower(canonical_name) = lower($3) OR EXISTS (SELECT 1 FROM unnest(aliases) a WHERE lower(a) = lower($3)))
            ORDER BY (lower(canonical_name) = lower($3)) DESC, created_at LIMIT 1""",
        space_id,
        kind,
        name,
    )
    if found is not None:
        return found
    row = await conn.fetchrow(
        """INSERT INTO entities (id, space_id, kind, canonical_name) VALUES ($1, $2, $3, $4)
           ON CONFLICT (space_id, kind, canonical_name) DO UPDATE SET canonical_name = EXCLUDED.canonical_name
           RETURNING id, (xmax = 0) AS inserted""",
        uuid7(),
        space_id,
        kind,
        name,
    )
    if row["inserted"]:
        await enqueue_embed(conn, "entities", row["id"])
    return row["id"]


def _cosine(a: list[float], b: list[float]) -> float:
    dot = sum(x * y for x, y in zip(a, b, strict=False))
    na, nb = math.sqrt(sum(x * x for x in a)), math.sqrt(sum(y * y for y in b))
    return dot / (na * nb) if na and nb else 0.0


async def same_value(active: asyncpg.Record, cand: FactCandidate) -> bool:
    """jsonb equality, else equal ``value_text`` ignoring case, else embedding cosine > 0.95 (10.4).
    An embedder failure counts as different (the candidate then waits for consolidation)."""
    if active["value"] == cand.value:
        return True
    if active["value_text"].strip().casefold() == cand.value_text.strip().casefold():
        return True
    try:
        a, b = await get_tei().embed([active["value_text"], cand.value_text])
    except Exception:  # noqa: BLE001 - TEIError or a transport error
        log.warning("embedder unavailable for fact value comparison; treating values as different")
        return False
    return _cosine(a, b) > SAME_VALUE_COSINE


def _valid_from(value: str | None) -> datetime | None:
    if not value:
        return None
    try:
        dt = datetime.fromisoformat(value)
    except ValueError:
        return None
    return dt if dt.tzinfo else dt.replace(tzinfo=UTC)


async def _insert_candidate(
    conn: asyncpg.Connection,
    packet: PacketRecord,
    session: asyncpg.Record,
    cand: FactCandidate,
    scope: tuple[str, str],
    subject_id: UUID,
    evidence: list[UUID],
    supersedes: UUID | None,
) -> UUID:
    fact_id = uuid7()
    await conn.execute(
        """INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text, confidence,
                             status, conflict, supersedes_id, valid_from, evidence_observation_ids,
                             learned_on_client_ids, extracted_by)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'candidate', $10, $11, $12, $13, $14, $15)""",
        fact_id,
        packet.space_id,
        scope[0],
        scope[1],
        subject_id,
        cand.predicate,
        cand.value if cand.value is not None else cand.value_text,
        cand.value_text,
        cand.confidence,
        supersedes is not None,
        supersedes,
        _valid_from(cand.valid_from),
        evidence,
        [session["client_id"]],
        extracted_by(packet),
    )
    await enqueue_embed(conn, "facts", fact_id)
    return fact_id


async def _confirm(
    conn: asyncpg.Connection,
    packet: PacketRecord,
    session: asyncpg.Record,
    active: asyncpg.Record,
    cand: FactCandidate,
    evidence: list[UUID],
) -> None:
    c_old = active["confidence"]
    confidence = c_old if active["pinned"] else 1 - (1 - c_old) * (1 - cand.confidence * 0.5)
    after = await conn.fetchrow(
        """UPDATE facts SET confirmations = confirmations + 1, last_confirmed_at = now(), confidence = $2,
                  evidence_observation_ids = ARRAY(SELECT DISTINCT unnest(evidence_observation_ids || $3::uuid[])),
                  learned_on_client_ids = ARRAY(SELECT DISTINCT unnest(learned_on_client_ids || $4::uuid[]))
            WHERE id = $1 RETURNING confidence, confirmations""",
        active["id"],
        min(1.0, confidence),
        evidence,
        [session["client_id"]],
    )
    await audit(
        conn,
        packet,
        "fact.confirm",
        "facts",
        active["id"],
        before={"confidence": c_old, "confirmations": active["confirmations"]},
        after={
            "confidence": after["confidence"],
            "confirmations": after["confirmations"],
            "value_text": cand.value_text,
        },
    )


# ---------- the applier ----------


async def apply_extract(conn: asyncpg.Connection, packet: PacketRecord, output: BaseModel) -> dict[str, int]:
    assert isinstance(output, ObserverV1)
    counts = {"observations": 0, "duplicates": 0, "fact_candidates": 0, "fact_confirmations": 0, "procedure_hints": 0}
    if output.skip:
        return counts
    if packet.session_id is None:
        raise ValueError("extract packet without a session")
    session = await builders.session_row(conn, packet.session_id)
    touched: set[tuple[str, str]] = set()

    seqs = {s for o in output.observations for s in o.source_event_seqs}
    seqs |= {s for f in output.fact_candidates for s in f.evidence_seqs}
    event_ids = {
        r["seq"]: r["id"]
        for r in await conn.fetch(
            "SELECT seq, id FROM events WHERE session_id = $1 AND seq = ANY($2::int[])", session["id"], list(seqs)
        )
    }

    # observations; seq -> ids of the observations (new or matched) citing that event, for fact evidence
    obs_by_seq: dict[int, list[UUID]] = {}
    packet_obs: list[UUID] = []
    first_new: UUID | None = None
    for obs in output.observations:
        scope = scope_for(obs.scope_hint, session)
        chash = content_hash(obs)
        subagent = any(e.startswith("subagent:") for e in obs.entities)
        norm_key = title_norm_key(obs.title, scope, session["harness"], subagent)
        obs_id = await conn.fetchval(
            f"""UPDATE observations SET importance = greatest(importance, $3)
                 WHERE id = (SELECT id FROM observations WHERE space_id = $1 AND content_hash = $2
                               AND created_at > now() - interval '{DEDUP_WINDOW}' ORDER BY created_at LIMIT 1)
                RETURNING id""",
            packet.space_id,
            chash,
            obs.importance,
        )
        if obs_id is None and norm_key is not None:
            obs_id = await conn.fetchval(
                f"""SELECT id FROM observations WHERE space_id = $1 AND title_norm_key = $2 AND archived_at IS NULL
                       AND created_at > now() - interval '{DEDUP_WINDOW}' ORDER BY created_at LIMIT 1""",
                packet.space_id,
                norm_key,
            )
            if obs_id is not None:
                await audit(
                    conn,
                    packet,
                    "obs.merge",
                    "observations",
                    obs_id,
                    after=obs.model_dump(mode="json"),
                    reason="same normalized title within 24 h; new observation skipped",
                )
        if obs_id is not None:
            counts["duplicates"] += 1
        else:
            obs_id = await _insert_observation(conn, packet, session, obs, scope, chash, norm_key, event_ids)
            counts["observations"] += 1
            first_new = first_new or obs_id
            touched.add(scope)
        packet_obs.append(obs_id)
        for s in obs.source_event_seqs:
            obs_by_seq.setdefault(s, []).append(obs_id)

    for cand in output.fact_candidates:
        scope = scope_for(cand.scope_hint, session)
        subject_id = await resolve_entity(conn, packet.space_id, cand.subject.kind, cand.subject.name)
        cited = [o for s in cand.evidence_seqs for o in obs_by_seq.get(s, [])]
        evidence = list(dict.fromkeys(cited or packet_obs))
        active = await conn.fetchrow(
            """SELECT * FROM facts WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND subject_id = $4
                  AND predicate = $5 AND status = 'active' FOR UPDATE""",
            packet.space_id,
            scope[0],
            scope[1],
            subject_id,
            cand.predicate,
        )
        if active is not None and await same_value(active, cand):
            await _confirm(conn, packet, session, active, cand, evidence)
            counts["fact_confirmations"] += 1
            touched.add(scope)
        else:
            await _insert_candidate(
                conn, packet, session, cand, scope, subject_id, evidence, active["id"] if active else None
            )
            counts["fact_candidates"] += 1

    hint_scope = scope_for(None, session)
    for hint in output.procedure_hints:
        await conn.execute(
            """INSERT INTO procedure_hints (id, space_id, scope_kind, scope_key, task_class, note, observation_id,
                                           session_id, packet_id)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)""",
            uuid7(),
            packet.space_id,
            hint_scope[0],
            hint_scope[1],
            hint.task_class,
            hint.note,
            first_new,
            session["id"],
            packet.id,
        )
        counts["procedure_hints"] += 1

    if output.needs_escalation:
        await audit(
            conn, packet, "packet.needs_escalation", "work_packets", packet.id, reason="runner asked for escalation"
        )

    for kind, key in sorted(touched):
        await notify_invalidate(conn, packet.space_id, kind, key)
    return counts


register("extract", apply_extract)
