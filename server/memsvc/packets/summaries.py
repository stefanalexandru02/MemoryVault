"""Applying ``summarize_prompt`` / ``summarize_session`` results (spec 10.5). Owner: ingest."""

from __future__ import annotations

import asyncpg
from pydantic import BaseModel

from memsvc.embed.jobs import enqueue_embed
from memsvc.ids import uuid7
from memsvc.invalidate import notify_invalidate
from memsvc.packets import builders
from memsvc.packets.appliers import register
from memsvc.packets.extract import extracted_by, scope_for
from memsvc.schemas.common import estimate_tokens
from memsvc.schemas.outputs import SummaryV1
from memsvc.schemas.work import PacketRecord

FIELDS = ("request", "investigated", "learned", "completed", "next_steps", "notes")


async def _apply_summary(conn: asyncpg.Connection, packet: PacketRecord, output: BaseModel, grain: str) -> dict[str, int]:
    assert isinstance(output, SummaryV1)
    if packet.session_id is None:
        raise ValueError(f"{packet.kind} packet without a session")
    session = await builders.session_row(conn, packet.session_id)
    scope = scope_for(None, session)
    prompt_no = packet.inputs.get("prompt_no") if grain == "prompt" else None
    period = (None, None) if grain == "prompt" else (session["started_at"], session["ended_at"])
    values = [getattr(output, f) for f in FIELDS]
    summary_id = uuid7()
    await conn.execute(
        """INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, session_id, prompt_no, period_start,
                                 period_end, request, investigated, learned, completed, next_steps, notes,
                                 extracted_by, token_estimate)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)""",
        summary_id,
        packet.space_id,
        scope[0],
        scope[1],
        grain,
        session["id"],
        prompt_no,
        *period,
        *values,
        extracted_by(packet),
        estimate_tokens(" ".join(v for v in values if v)),
    )
    await enqueue_embed(conn, "summaries", summary_id)
    await notify_invalidate(conn, packet.space_id, scope[0], scope[1])
    return {"summaries": 1}


async def apply_summarize_prompt(conn: asyncpg.Connection, packet: PacketRecord, output: BaseModel) -> dict[str, int]:
    return await _apply_summary(conn, packet, output, "prompt")


async def apply_summarize_session(conn: asyncpg.Connection, packet: PacketRecord, output: BaseModel) -> dict[str, int]:
    return await _apply_summary(conn, packet, output, "session")


register("summarize_prompt", apply_summarize_prompt)
register("summarize_session", apply_summarize_session)
