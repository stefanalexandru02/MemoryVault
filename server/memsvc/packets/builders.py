"""Packet inputs for extract / summarize packets (spec 9.2), built once at creation. Owner: ingest."""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Any
from uuid import UUID

import asyncpg

from memsvc.packets.queue import create_packet
from memsvc.prompts import registry
from memsvc.schemas.packets import (
    ExtractInputs,
    PacketEvent,
    PacketSessionInfo,
    PromptSummaryIn,
    RecentTitle,
    SummarizePromptInputs,
    SummarizeSessionInputs,
)

log = logging.getLogger(__name__)

# Prompt names per packet kind, first loaded name wins (the prompt YAML files are owned by another module).
PROMPT_NAMES: dict[str, tuple[str, ...]] = {
    "extract": ("observer.code",),
    "summarize_prompt": ("summary.prompt", "summary"),
    "summarize_session": ("summary.session", "summary"),
}
OUTPUT_SCHEMAS = {"extract": "observer.v1", "summarize_prompt": "summary.v1", "summarize_session": "summary.v1"}

PROFILE_MAX_CHARS = 1600  # ~400 tokens (estimate_tokens = chars/4)
RECENT_TITLES = 20
KNOWN_ENTITIES = 50


@dataclass(frozen=True)
class PromptChoice:
    prompt_ref: str
    model_hint: str
    output_schema: str


def prompt_for(kind: str) -> PromptChoice:
    """Pinned ``name@version`` of the newest loaded prompt for ``kind``.

    When none is loaded the bare name is stored and pinned at lease time (``queue.lease``) instead, so
    ingest never fails because the registry is empty.
    """
    for name in PROMPT_NAMES[kind]:
        try:
            spec = registry.get(name)
        except KeyError:
            continue
        return PromptChoice(spec.prompt_ref, spec.model_hint, spec.output_schema)
    log.warning("no prompt loaded for packet kind %s; storing unpinned ref", kind)
    return PromptChoice(PROMPT_NAMES[kind][0], "sonnet", OUTPUT_SCHEMAS[kind])


async def session_row(conn: asyncpg.Connection, session_id: UUID) -> asyncpg.Record:
    row = await conn.fetchrow(
        """SELECT s.*, c.device_id, c.platform FROM sessions s JOIN clients c ON c.id = s.client_id
            WHERE s.id = $1""",
        session_id,
    )
    if row is None:
        raise LookupError(f"session {session_id} not found")
    return row


def session_info(session: asyncpg.Record) -> PacketSessionInfo:
    return PacketSessionInfo(
        harness=session["harness"],
        project_key=session["project_key"],
        device=session["device_id"],
        started_at=session["started_at"],
    )


def _text(value: Any) -> str | None:
    return value if isinstance(value, str) else None


def packet_event(row: asyncpg.Record) -> dict[str, Any]:
    """One ``events`` row as a PacketEvent dict. ``agent_id``/``agent_type`` are added for subagent events
    (spec 8.3: the extractor tags their observations ``subagent:<agent_type>``)."""
    p: dict[str, Any] = row["payload"] or {}
    kind = row["kind"]
    ev = PacketEvent(seq=row["seq"], kind=kind, at=row["occurred_at"])
    if kind in ("tool_use", "tool_result"):
        ev.tool, ev.input, ev.output = p.get("tool_name"), p.get("tool_input"), p.get("tool_response")
    elif kind == "tool_failure":
        ev.tool, ev.input, ev.output = p.get("tool_name"), p.get("tool_input"), p.get("error")
    elif kind == "user_prompt":
        ev.text = _text(p.get("prompt"))
    elif kind in ("assistant_message", "subagent_stop"):
        ev.text = _text(p.get("last_assistant_message"))
    else:  # system, note
        ev.text = _text(p.get("text"))
        if ev.text is None and p:
            ev.input = p
    out = ev.model_dump(mode="json", exclude_none=True)
    for key in ("agent_id", "agent_type"):
        if p.get(key):
            out[key] = p[key]
    return out


async def _titles(conn: asyncpg.Connection, sql: str, *args: Any) -> list[RecentTitle]:
    rows = await conn.fetch(sql, *args)
    return [RecentTitle(id=r["id"], type=r["type"], title=r["title"]) for r in rows]


async def build_extract_inputs(conn: asyncpg.Connection, session: asyncpg.Record, window_no: int) -> dict[str, Any]:
    events = await conn.fetch(
        "SELECT seq, kind, occurred_at, payload FROM events WHERE session_id = $1 AND window_no = $2 ORDER BY seq",
        session["id"],
        window_no,
    )
    recent = await _titles(
        conn,
        """SELECT id, type, title FROM (
             SELECT id, type, title, created_at FROM observations
              WHERE session_id = $1 AND archived_at IS NULL ORDER BY created_at DESC, id DESC LIMIT $2) t
            ORDER BY created_at, id""",
        session["id"],
        RECENT_TITLES,
    )
    profile = await conn.fetchval(
        """SELECT body_md FROM profiles WHERE space_id = $1
              AND ((scope_kind = 'project' AND scope_key = $2) OR scope_kind = 'global')
            ORDER BY (scope_kind = 'project') DESC LIMIT 1""",
        session["space_id"],
        session["project_key"] or "",
    )
    entities = await conn.fetch(
        "SELECT canonical_name FROM entities WHERE space_id = $1 ORDER BY created_at DESC LIMIT $2",
        session["space_id"],
        KNOWN_ENTITIES,
    )
    inputs = ExtractInputs(
        session=session_info(session),
        window_no=window_no,
        events=[],
        recent_titles=recent,
        profile_md=(profile or "")[:PROFILE_MAX_CHARS],
        known_entities=[r["canonical_name"] for r in entities],
    ).model_dump(mode="json")
    inputs["events"] = [packet_event(e) for e in events]
    return inputs


async def build_summarize_prompt_inputs(
    conn: asyncpg.Connection, session: asyncpg.Record, prompt_no: int, window_nos: list[int]
) -> dict[str, Any]:
    prompt = None
    if prompt_no > 0:
        payload = await conn.fetchval(
            "SELECT payload FROM events WHERE session_id = $1 AND kind = 'user_prompt' ORDER BY seq OFFSET $2 LIMIT 1",
            session["id"],
            prompt_no - 1,
        )
        prompt = _text((payload or {}).get("prompt"))
    last = await conn.fetchval(
        """SELECT payload FROM events WHERE session_id = $1 AND window_no = ANY($2::int[])
              AND kind = 'assistant_message' ORDER BY seq DESC LIMIT 1""",
        session["id"],
        window_nos,
    )
    titles = await _titles(
        conn,
        """SELECT id, type, title FROM observations
            WHERE session_id = $1 AND window_no = ANY($2::int[]) AND archived_at IS NULL ORDER BY created_at, id""",
        session["id"],
        window_nos,
    )
    return SummarizePromptInputs(
        session=session_info(session),
        prompt_no=prompt_no,
        prompt=prompt,
        observation_titles=titles,
        last_assistant_message=_text((last or {}).get("last_assistant_message")),
    ).model_dump(mode="json")


async def build_summarize_session_inputs(conn: asyncpg.Connection, session: asyncpg.Record) -> dict[str, Any] | None:
    """None when the session has no prompt summaries (nothing to fold)."""
    rows = await conn.fetch(
        """SELECT prompt_no, request, investigated, learned, completed, next_steps, notes FROM summaries
            WHERE session_id = $1 AND grain = 'prompt' ORDER BY prompt_no NULLS LAST, created_at""",
        session["id"],
    )
    if not rows:
        return None
    return SummarizeSessionInputs(
        session=session_info(session),
        prompt_summaries=[PromptSummaryIn(**dict(r)) for r in rows],
    ).model_dump(mode="json")


async def create_session_packet(
    conn: asyncpg.Connection, session: asyncpg.Record, kind: str, idempotency_key: str, inputs: dict[str, Any]
) -> UUID | None:
    """Create an extract/summarize packet for ``session``; its client is the preferred runner (9.3)."""
    choice = prompt_for(kind)
    return await create_packet(
        conn,
        space_id=session["space_id"],
        kind=kind,
        prompt_ref=choice.prompt_ref,
        model_hint=choice.model_hint,
        output_schema=choice.output_schema,
        inputs=inputs,
        idempotency_key=idempotency_key,
        session_id=session["id"],
        preferred_client_id=session["client_id"],
    )
