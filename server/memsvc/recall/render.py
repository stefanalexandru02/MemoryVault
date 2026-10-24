"""Deterministic markdown rendering (spec 11.5): citation tags, slot sections, the progressive-disclosure
index (claude-mem's glyphs, code mode), since-you-were-here, memory bodies and explain chains.

Everything here is a pure function of its inputs (UTC dates, stable ordering), so the same slots always
render to the same bytes. The MCP tools reuse ``index_table`` / ``memory_items_markdown`` /
``explain_markdown`` so pull-mode output matches the SessionStart block.
"""

from __future__ import annotations

import re
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Any
from uuid import UUID

from memsvc.ids import short_id
from memsvc.schemas.common import Scope
from memsvc.schemas.memories import (
    ExplainResponse,
    FactFull,
    IndexRow,
    ObservationFull,
    ProcedureFull,
    Provenance,
    SummaryFull,
)
from memsvc.schemas.recall import (
    FactItem,
    ObservationItem,
    ProcedureItem,
    RecallSlots,
    SummaryItem,
    WorkStateList,
)

# claude-mem plugin/modes/code.json observation type glyphs.
GLYPHS: dict[str, str] = {
    "decision": "⚖",
    "bugfix": "●",
    "feature": "◆",
    "refactor": "↻",
    "change": "✓",
    "discovery": "○",
    "security_alert": "⚠",
    "security_note": "⚷",
    "sensitive": "⊘",
}
TABLE_GLYPHS: dict[str, str] = {"summaries": "§", "facts": "≡", "procedures": "¶"}
DEFAULT_GLYPH = "•"
LEGEND = "_Legend: " + " · ".join(f"{g} {t}" for t, g in GLYPHS.items()) + " · § summary_"
INDEX_HEADER = "| id | when | t | title | ~tok |\n|---|---|---|---|---|"
EXCERPT_CHARS = 160
NARRATIVE_CHARS = 600

TAG_KIND = {"observations": "obs", "summaries": "sum", "facts": "fact", "procedures": "proc"}


def utc(dt: datetime) -> datetime:
    return dt.astimezone(UTC) if dt.tzinfo else dt.replace(tzinfo=UTC)


def fmt_date(dt: datetime | None) -> str:
    return utc(dt).strftime("%Y-%m-%d") if dt else "?"


def fmt_md(dt: datetime | None) -> str:
    return utc(dt).strftime("%m-%d") if dt else "?"


def fmt_when(dt: datetime | None) -> str:
    return utc(dt).strftime("%m-%d %H:%M") if dt else "?"


def one_line(text: str | None) -> str:
    return re.sub(r"\s+", " ", text or "").strip()


def excerpt(text: str | None, limit: int = EXCERPT_CHARS) -> str:
    s = one_line(text)
    return s if len(s) <= limit else s[: limit - 1].rstrip() + "…"


def cell(text: str | None) -> str:
    return one_line(text).replace("|", "\\|")


def glyph(table: str, type_: str | None) -> str:
    if table == "observations":
        return GLYPHS.get(type_ or "", DEFAULT_GLYPH)
    return TABLE_GLYPHS.get(table, DEFAULT_GLYPH)


# ---------- citation tags ----------


def _tag(kind: str, ident: UUID, *parts: str | None) -> str:
    bits = [f"{kind}#{short_id(ident)}"] + [p for p in parts if p]
    return "[" + " · ".join(bits) + "]"


def fact_tag(id_: UUID, confidence: float, devices: Sequence[str], first_seen: datetime | None) -> str:
    return _tag("fact", id_, f"{confidence:.2f}", "+".join(devices) or None, fmt_date(first_seen))


def obs_tag(id_: UUID, device: str | None, created_at: datetime | None) -> str:
    return _tag("obs", id_, device, fmt_date(created_at))


def summary_tag(id_: UUID, device: str | None, created_at: datetime | None) -> str:
    return _tag("sum", id_, device, fmt_date(created_at))


def procedure_tag(id_: UUID, version: int, created_at: datetime | None) -> str:
    return _tag("proc", id_, f"v{version}", fmt_date(created_at))


# ---------- slot lines ----------


def fact_line(f: FactItem) -> str:
    subject = "" if (f.subject_kind or "user") == "user" else f"{f.subject} "
    return f"- {subject}{f.predicate}: {one_line(f.value_text)} {f.citation or ''}".rstrip()


def procedure_line(p: ProcedureItem) -> str:
    if p.success_count or p.last_used_at:
        usage = f"used {p.success_count}×" + (f", last {fmt_md(p.last_used_at)}" if p.last_used_at else "")
    else:
        usage = f"v{p.version}"
    return f"- **{one_line(p.name)}** ({usage}): {excerpt(p.body_md)} {p.citation or ''}".rstrip()


def observation_block(o: ObservationItem) -> str:
    head = f"- {glyph('observations', o.type)} **{one_line(o.title)}**"
    if o.subtitle:
        head += f" — {one_line(o.subtitle)}"
    lines = [f"{head} {o.citation or ''}".rstrip()]
    if o.narrative:
        lines.append(f"  {excerpt(o.narrative, NARRATIVE_CHARS)}")
    lines.extend(f"  - {one_line(x)}" for x in o.facts)
    return "\n".join(lines)


def summary_line(s: SummaryItem) -> str:
    who = f" {s.device_id}" if s.device_id else ""
    text = f'"{excerpt(s.request, 120)}"' if s.request else excerpt(s.learned or s.completed, 120)
    nxt = f" → next: {excerpt(s.next_steps, 160)}" if s.next_steps else ""
    return f"- {fmt_date(s.created_at)}{who} · {text}{nxt} {s.citation or ''}".rstrip()


def scope_label(scope: Scope) -> str:
    if scope.kind == "global":
        return "global"
    if scope.kind == "project":
        return scope.key.rstrip("/").rsplit("/", 1)[-1] or scope.key
    return scope.key


_DONE = {"done", "complete", "completed", "x", "✓", "true", "yes"}
_OPEN = {"todo", "open", "pending", "", "☐", "false", "no", "wip"}


def _state_mark(value: Any) -> str:
    if isinstance(value, bool):
        return "✓" if value else "☐"
    text = one_line(str(value))
    if text.lower() in _DONE:
        return "✓"
    if text.lower() in _OPEN:
        return "☐"
    return excerpt(text, 40)


def work_state_line(w: WorkStateList) -> str:
    items = " · ".join(f"{one_line(k)} {_state_mark(v)}" for k, v in w.fields.items())
    return f"- {w.list_name}/{scope_label(w.scope)}: {items}"


# ---------- index ----------


@dataclass(frozen=True)
class IndexEntry:
    id: UUID
    when: datetime
    glyph: str
    title: str
    tokens: int
    anchor: bool = False


def entry_from_row(r: IndexRow) -> IndexEntry:
    return IndexEntry(r.id, r.created_at, glyph(r.table, r.type), r.title, r.token_estimate, r.is_anchor)


def entry_from_observation(o: ObservationItem) -> IndexEntry:
    return IndexEntry(o.id, o.created_at, glyph("observations", o.type), o.title, o.token_estimate)


def index_line(e: IndexEntry, full_ids: bool = False) -> str:
    ref = str(e.id) if full_ids else short_id(e.id)
    if e.anchor:
        ref = f"**▶ {ref}**"
    return f"| {ref} | {fmt_when(e.when)} | {e.glyph} | {cell(e.title)} | {e.tokens} |"


def index_table(entries: Sequence[IndexEntry], *, full_ids: bool = False, legend: bool = True) -> str:
    if not entries:
        return ""
    lines = [INDEX_HEADER, *(index_line(e, full_ids) for e in entries)]
    if legend:
        lines.append(LEGEND)
    return "\n".join(lines)


def index_rows_markdown(rows: Sequence[IndexRow], *, full_ids: bool = True) -> str:
    """Search/timeline/by-file output for the MCP tools (full ids so ``memory_get`` can take them)."""
    if not rows:
        return "_No matching memories._"
    return index_table([entry_from_row(r) for r in rows], full_ids=full_ids)


# ---------- sections ----------


def section(title: str, body: str) -> str:
    return f"### {title}\n{body}" if body.strip() else ""


def join_sections(parts: Sequence[str]) -> str:
    return "\n".join(p for p in parts if p)


def profile_body(slots: RecallSlots, pinned: Sequence[FactItem]) -> str:
    lines: list[str] = []
    if slots.profile and slots.profile.body_md.strip():
        lines.append(slots.profile.body_md.strip())
    lines.extend(fact_line(f) for f in pinned)
    return "\n".join(lines)


def recall_markdown(slots: RecallSlots, expand: int) -> str:
    """``rendered_markdown`` of ``POST /v1/recall``."""
    pinned = [f for f in slots.facts if f.pinned]
    others = [f for f in slots.facts if not f.pinned]
    expanded = [o for o in slots.observations if o.expanded][:expand]
    rest = [o for o in slots.observations if not o.expanded]
    obs_parts = [observation_block(o) for o in expanded]
    if rest:
        obs_parts.append(index_table([entry_from_observation(o) for o in rest]))
    body = join_sections(
        [
            section("Profile", profile_body(slots, pinned)),
            section("Facts", "\n".join(fact_line(f) for f in others)),
            section("Open work", "\n".join(work_state_line(w) for w in slots.work_state)),
            section("Relevant procedures", "\n".join(procedure_line(p) for p in slots.procedures)),
            section("Observations", "\n".join(obs_parts)),
            section("Summaries", "\n".join(summary_line(s) for s in slots.summaries)),
        ]
    )
    return "## Memory\n" + (body or "_No relevant memories._")


def context_sections(slots: RecallSlots) -> list[str]:
    """Cached portion of the SessionStart block (everything except since-you-were-here)."""
    pinned = [f for f in slots.facts if f.pinned]
    return [
        section("Profile", profile_body(slots, pinned)),
        section("Open work", "\n".join(work_state_line(w) for w in slots.work_state)),
        section("Relevant procedures", "\n".join(procedure_line(p) for p in slots.procedures)),
        section(
            "Recent observations (fetch with memory_get)",
            index_table([entry_from_observation(o) for o in slots.observations]),
        ),
        section("Recent sessions", "\n".join(summary_line(s) for s in slots.summaries)),
    ]


@dataclass(frozen=True)
class SinceRow:
    started_at: datetime
    device: str
    request: str | None
    next_steps: str | None


def since_section(device: str | None, last: datetime | None, rows: Sequence[SinceRow]) -> str:
    if not rows:
        return ""
    head = f"Since you were here ({device or 'this device'}, last {fmt_date(last) if last else 'none in 14 days'})"
    lines = []
    for r in rows:
        what = f'"{excerpt(r.request, 120)}"' if r.request else "(no summary yet)"
        nxt = f" → next: {excerpt(r.next_steps, 160)}" if r.next_steps else ""
        lines.append(f"- {fmt_date(r.started_at)} {r.device} · {what}{nxt}")
    return section(head, "\n".join(lines))


def assemble_context(cached_body: str, since: str) -> str:
    """``## Memory`` + Profile + live since-you-were-here + the rest of the cached sections."""
    body = cached_body
    if since:
        if body.startswith("### Profile\n"):
            nxt = body.find("\n### ", len("### Profile\n"))
            body = body + "\n" + since if nxt < 0 else body[:nxt] + "\n" + since + body[nxt:]
        else:
            body = since + ("\n" + body if body else "")
    return "## Memory\n" + (body or "_No memories for this project yet._")


# ---------- memory bodies (memory_get) and explain ----------


def _scope_text(s: Scope) -> str:
    return s.as_text()


def memory_item_markdown(item: ObservationFull | SummaryFull | FactFull | ProcedureFull) -> str:
    if isinstance(item, ObservationFull):
        head = f"#### {glyph('observations', item.type)} {one_line(item.title)} {obs_tag(item.id, None, item.created_at)}"
        lines = [head, f"`{item.id}` · {item.type} · {_scope_text(item.scope)} · {fmt_when(item.created_at)}"]
        if item.subtitle:
            lines.append(f"_{one_line(item.subtitle)}_")
        if item.narrative:
            lines.append(item.narrative.strip())
        lines.extend(f"- {one_line(x)}" for x in item.facts)
        if item.files:
            lines.append("Files: " + ", ".join(item.files))
        return "\n".join(lines)
    if isinstance(item, SummaryFull):
        lines = [
            f"#### § {item.grain} summary {summary_tag(item.id, None, item.created_at)}",
            f"`{item.id}` · {_scope_text(item.scope)} · {fmt_when(item.created_at)}",
        ]
        for label in ("request", "investigated", "learned", "completed", "next_steps", "notes"):
            value = getattr(item, label)
            if value:
                lines.append(f"**{label}**: {value.strip()}")
        return "\n".join(lines)
    if isinstance(item, FactFull):
        tag = fact_tag(item.id, item.confidence, [], item.first_seen_at)
        return "\n".join(
            [
                f"#### ≡ {item.subject.canonical_name} {item.predicate} {tag}",
                f"`{item.id}` · {item.status} · {_scope_text(item.scope)} · confirmations {item.confirmations}",
                one_line(item.value_text),
            ]
        )
    lines = [
        f"#### ¶ {one_line(item.name)} {procedure_tag(item.id, item.version, item.created_at)}",
        f"`{item.id}` · {item.task_class} · {item.status} · {_scope_text(item.scope)}",
        f"When: {one_line(item.trigger_text)}",
        item.body_md.strip(),
    ]
    return "\n".join(lines)


def memory_items_markdown(items: Sequence[ObservationFull | SummaryFull | FactFull | ProcedureFull]) -> str:
    return "\n\n".join(memory_item_markdown(i) for i in items) or "_Nothing found._"


def _provenance_lines(p: Provenance | None, indent: str = "") -> list[str]:
    if p is None:
        return []
    lines = []
    if p.session:
        s = p.session
        lines.append(
            f"{indent}- session `{s.id}` · {s.harness} · {s.project_key or '-'} · {s.device_id or '-'} · "
            f"{fmt_when(s.started_at)}"
        )
    if p.event_seqs:
        lines.append(f"{indent}- events seq {', '.join(str(x) for x in p.event_seqs)}")
    if p.extracted_by:
        eb = ", ".join(f"{k}={p.extracted_by[k]}" for k in sorted(p.extracted_by))
        lines.append(f"{indent}- extracted by {eb}")
    return lines


def explain_markdown(e: ExplainResponse) -> str:
    lines = [memory_item_markdown(e.item), "", "**Provenance**"]
    lines.extend(_provenance_lines(e.provenance) or (["- (none recorded)"] if not e.evidence else []))
    for ev in e.evidence:
        lines.append(f"- evidence {obs_tag(ev.id, None, ev.created_at)} {one_line(ev.title)} ({ev.type})")
        lines.extend(_provenance_lines(ev.provenance, "  "))
    if e.audit:
        lines += ["", "**Audit**"]
        for a in e.audit:
            reason = f": {one_line(a.reason)}" if a.reason else ""
            lines.append(f"- {fmt_when(a.created_at)} {a.action} by {a.actor}{reason}")
    return "\n".join(lines)
