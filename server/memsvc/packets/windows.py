"""Window assignment and closing (spec 9.1), and the deferred summarize packets. Owner: ingest.

State lives in ``session_windows`` (migration 0100); the ``sessions`` row is the lock every change takes.

- ``window_no`` starts at 1. A window closes on its ``window_tool_uses``-th (8) ``tool_use``/``tool_failure``
  event, on an ``assistant_message`` (Stop), on ``POST /v1/sessions/{id}/end``, or when the worker sweep finds
  it idle for ``window_idle_seconds`` (90 s). Exactly one rule closes each window (``close_reason``).
- Closing a window with content creates ``extract:<session>:<window>``. Windows holding only
  ``user_prompt``/``system`` events create no packet (and are never closed by the idle sweep, so a prompt is
  not cut off from the tool calls that follow it).
- A Stop owes ``summarize_prompt:<session>:<prompt_no>``, and ``end`` owes ``summarize_session:<session>``.
  Packet inputs are immutable, so these are created once the packets they summarise have settled
  (done/failed/cancelled): the prompt summary then carries the window observations' titles and the session
  summary the prompt summaries. :func:`after_packet_settled` and the sweep trigger that.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from uuid import UUID

import asyncpg

from memsvc.config import get_settings
from memsvc.packets import builders

TOOL_KINDS = frozenset({"tool_use", "tool_failure"})
NO_CONTENT_KINDS = frozenset({"user_prompt", "system"})
SETTLED = ("done", "failed", "cancelled")


@dataclass
class NewEvent:
    id: UUID
    seq: int
    kind: str


@dataclass
class Assignment:
    window_no: int  # window of the newest assigned event (the open window when nothing was assigned)
    closed: list[int] = field(default_factory=list)  # windows closed by this call
    packets: list[str] = field(default_factory=list)  # kinds of packets created


async def lock_session(conn: asyncpg.Connection, session_id: UUID) -> asyncpg.Record | None:
    """Lock the session row (the window-state mutex) and return it joined with its client."""
    found = await conn.fetchval("SELECT id FROM sessions WHERE id = $1 FOR UPDATE", session_id)
    if found is None:
        return None
    return await builders.session_row(conn, session_id)


async def open_window(conn: asyncpg.Connection, session: asyncpg.Record) -> asyncpg.Record:
    """The session's open window, created (numbered after the last one) when there is none."""
    row = await conn.fetchrow(
        "SELECT * FROM session_windows WHERE session_id = $1 AND closed_at IS NULL", session["id"]
    )
    if row is not None:
        return row
    last = await conn.fetchrow(
        "SELECT window_no, prompt_no FROM session_windows WHERE session_id = $1 ORDER BY window_no DESC LIMIT 1",
        session["id"],
    )
    return await conn.fetchrow(
        """INSERT INTO session_windows (space_id, session_id, window_no, prompt_no)
           VALUES ($1, $2, $3, $4) RETURNING *""",
        session["space_id"],
        session["id"],
        (last["window_no"] + 1) if last else 1,
        last["prompt_no"] if last else 0,
    )


async def assign_events(conn: asyncpg.Connection, session: asyncpg.Record, events: list[NewEvent]) -> Assignment:
    """Assign newly inserted events (any order; processed by ``seq``) to windows, closing windows as the
    rules fire. The caller holds the session lock (:func:`lock_session`)."""
    limit = get_settings().window_tool_uses
    w = await open_window(conn, session)
    result = Assignment(window_no=w["window_no"])
    if not events:
        return result
    window_no, prompt_no = w["window_no"], w["prompt_no"]
    tool_uses, count, has_content = w["tool_uses"], w["event_count"], w["has_content"]
    segment: list[UUID] = []

    async def flush() -> None:
        await conn.execute(
            "UPDATE events SET window_no = $3 WHERE session_id = $1 AND id = ANY($2::uuid[])",
            session["id"],
            segment,
            window_no,
        )
        await conn.execute(
            """UPDATE session_windows SET prompt_no = $3, tool_uses = $4, event_count = $5, has_content = $6,
                      last_event_at = now()
                WHERE session_id = $1 AND window_no = $2""",
            session["id"],
            window_no,
            prompt_no,
            tool_uses,
            count,
            has_content,
        )
        segment.clear()

    for ev in sorted(events, key=lambda e: e.seq):
        segment.append(ev.id)
        count += 1
        result.window_no = window_no
        if ev.kind == "user_prompt":
            prompt_no += 1
        if ev.kind in TOOL_KINDS:
            tool_uses += 1
        if ev.kind not in NO_CONTENT_KINDS:
            has_content = True
        reason = "stop" if ev.kind == "assistant_message" else ("tool_uses" if tool_uses >= limit else None)
        if reason is None:
            continue
        await flush()
        result.packets += await close_window(conn, session, window_no, reason)
        result.closed.append(window_no)
        w = await open_window(conn, session)
        window_no, prompt_no = w["window_no"], w["prompt_no"]
        tool_uses, count, has_content = 0, 0, False
    if segment:
        await flush()
    return result


async def close_window(conn: asyncpg.Connection, session: asyncpg.Record, window_no: int, reason: str) -> list[str]:
    """Close one open window; returns the kinds of packets created. Caller holds the session lock."""
    w = await conn.fetchrow(
        """UPDATE session_windows SET closed_at = now(), close_reason = $3, prompt_summary_pending = ($3 = 'stop')
            WHERE session_id = $1 AND window_no = $2 AND closed_at IS NULL
        RETURNING has_content, prompt_no""",
        session["id"],
        window_no,
        reason,
    )
    if w is None:
        return []
    created: list[str] = []
    if w["has_content"]:
        key = f"extract:{session['id']}:{window_no}"
        inputs = await builders.build_extract_inputs(conn, session, window_no)
        if await builders.create_session_packet(conn, session, "extract", key, inputs):
            created.append("extract")
        await conn.execute(
            """UPDATE session_windows SET extract_packet_id = (SELECT id FROM work_packets WHERE idempotency_key = $3)
                WHERE session_id = $1 AND window_no = $2""",
            session["id"],
            window_no,
            key,
        )
    if reason == "stop":
        created += await maybe_create_prompt_summary(conn, session, w["prompt_no"])
    return created


async def maybe_create_prompt_summary(conn: asyncpg.Connection, session: asyncpg.Record, prompt_no: int) -> list[str]:
    """Create the owed ``summarize_prompt`` packet once the prompt's extract packets have settled."""
    windows = await conn.fetch(
        """SELECT w.window_no, w.prompt_summary_pending, p.status AS extract_status
             FROM session_windows w LEFT JOIN work_packets p ON p.id = w.extract_packet_id
            WHERE w.session_id = $1 AND w.prompt_no = $2 AND w.closed_at IS NOT NULL""",
        session["id"],
        prompt_no,
    )
    if not any(w["prompt_summary_pending"] for w in windows):
        return []
    if any(w["extract_status"] is not None and w["extract_status"] not in SETTLED for w in windows):
        return []
    window_nos = [w["window_no"] for w in windows]
    await conn.execute(
        "UPDATE session_windows SET prompt_summary_pending = false WHERE session_id = $1 AND window_no = ANY($2::int[])",
        session["id"],
        window_nos,
    )
    inputs = await builders.build_summarize_prompt_inputs(conn, session, prompt_no, window_nos)
    key = f"summarize_prompt:{session['id']}:{prompt_no}"
    return ["summarize_prompt"] if await builders.create_session_packet(conn, session, "summarize_prompt", key, inputs) else []


async def maybe_create_session_summary(conn: asyncpg.Connection, session: asyncpg.Record) -> list[str]:
    """Create the owed ``summarize_session`` packet once nothing it folds is still in flight."""
    pending = await conn.fetchval("SELECT session_summary_pending FROM sessions WHERE id = $1", session["id"])
    if not pending:
        return []
    busy = await conn.fetchval(
        """SELECT EXISTS (SELECT 1 FROM session_windows WHERE session_id = $1 AND prompt_summary_pending)
               OR EXISTS (SELECT 1 FROM work_packets WHERE session_id = $1
                            AND kind IN ('extract', 'summarize_prompt') AND status IN ('pending', 'leased'))""",
        session["id"],
    )
    if busy:
        return []
    await conn.execute("UPDATE sessions SET session_summary_pending = false WHERE id = $1", session["id"])
    inputs = await builders.build_summarize_session_inputs(conn, session)
    if inputs is None:
        return []
    key = f"summarize_session:{session['id']}"
    return ["summarize_session"] if await builders.create_session_packet(conn, session, "summarize_session", key, inputs) else []


async def end_session(conn: asyncpg.Connection, session: asyncpg.Record, reason: str | None) -> tuple[bool, list[str]]:
    """``POST /v1/sessions/{id}/end``: close the open window, owe the session summary. Caller holds the lock.

    Returns (an open window was closed, kinds of packets created)."""
    await conn.execute(
        "UPDATE sessions SET ended_at = now(), end_reason = $2, session_summary_pending = true WHERE id = $1",
        session["id"],
        reason,
    )
    created: list[str] = []
    w = await conn.fetchrow(
        "SELECT window_no, event_count FROM session_windows WHERE session_id = $1 AND closed_at IS NULL", session["id"]
    )
    closed = False
    if w is not None and w["event_count"] > 0:
        created += await close_window(conn, session, w["window_no"], "end")
        closed = True
    elif w is not None:  # an empty open window (left by the last close): drop it rather than close it empty
        await conn.execute(
            "DELETE FROM session_windows WHERE session_id = $1 AND window_no = $2", session["id"], w["window_no"]
        )
    created += await maybe_create_session_summary(conn, session)
    return closed, created


async def after_packet_settled(conn: asyncpg.Connection, kind: str, session_id: UUID | None, inputs: dict) -> list[str]:
    """Call when an extract/summarize packet reached done/failed/cancelled, in the same transaction."""
    if session_id is None or kind not in ("extract", "summarize_prompt"):
        return []
    session = await lock_session(conn, session_id)
    if session is None:
        return []
    created: list[str] = []
    if kind == "extract":
        prompt_no = await conn.fetchval(
            "SELECT prompt_no FROM session_windows WHERE session_id = $1 AND window_no = $2",
            session_id,
            inputs.get("window_no"),
        )
        if prompt_no is not None:
            created += await maybe_create_prompt_summary(conn, session, prompt_no)
    created += await maybe_create_session_summary(conn, session)
    return created


async def close_idle_windows(conn: asyncpg.Connection) -> int:
    """Close every open window whose newest event is older than ``window_idle_seconds`` (90 s).

    Called by the worker sweep every 30 s inside ``system_tx``. Closing a window with content creates
    its ``extract`` packet (idempotency key ``extract:<session>:<window>``); windows holding only
    ``user_prompt``/``system`` events create none (and stay open). Returns the number of windows closed.

    The same sweep also fails expired leases that used their last attempt (``queue.expire_leases``) and
    creates summarize packets whose prerequisites settled without passing through the result route.
    """
    from memsvc.packets.queue import expire_leases

    idle = get_settings().window_idle_seconds
    rows = await conn.fetch(
        """SELECT s.id FROM sessions s
            WHERE EXISTS (SELECT 1 FROM session_windows w WHERE w.session_id = s.id AND w.closed_at IS NULL
                            AND w.has_content AND w.last_event_at < now() - make_interval(secs => $1))
            ORDER BY s.id FOR UPDATE OF s SKIP LOCKED""",
        float(idle),
    )
    closed = 0
    for r in rows:
        session = await builders.session_row(conn, r["id"])
        w = await conn.fetchrow(
            """SELECT window_no FROM session_windows WHERE session_id = $1 AND closed_at IS NULL
                  AND has_content AND last_event_at < now() - make_interval(secs => $2)""",
            r["id"],
            float(idle),
        )
        if w is not None:
            await close_window(conn, session, w["window_no"], "idle")
            closed += 1

    await expire_leases(conn)

    owed = await conn.fetch(
        """SELECT DISTINCT session_id, prompt_no FROM session_windows WHERE prompt_summary_pending
           UNION ALL
           SELECT id, NULL FROM sessions WHERE session_summary_pending"""
    )
    for r in owed:
        session = await lock_session(conn, r["session_id"])
        if session is None:
            continue
        if r["prompt_no"] is not None:
            await maybe_create_prompt_summary(conn, session, r["prompt_no"])
        else:
            await maybe_create_session_summary(conn, session)
    return closed
