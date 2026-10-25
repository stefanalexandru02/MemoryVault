//! Deterministic markdown rendering (spec 11.5): citation tags, slot sections, the
//! progressive-disclosure index (claude-mem's code-mode glyphs), since-you-were-here, memory bodies
//! and the provenance and supersession trace.
//!
//! Everything here is a pure function of its inputs (UTC dates, stable ordering), so the same slots
//! always render to the same bytes. The text goes into a model's context, so it is spare: ids are the
//! shortest ending that tells the rows shown apart, an index names each day once, and the glyph legend
//! belongs to the SessionStart block alone. The MCP tools reuse [`search_rows_markdown`],
//! [`timeline_rows_markdown`], [`memory_item_markdowns`] and [`trace_markdown`] so pull-mode output matches
//! that block.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use memvault_core::ids::{short_id, short_ids};
use memvault_core::schemas::common::{MemoryTable, Scope, ScopeKind, normalize_title};
use memvault_core::schemas::memories::{
    DocumentFull, ExplainResponse, HistoryResponse, IndexRow, MemoryItem, Provenance, procedure_caution,
};
use memvault_core::schemas::recall::{
    DocumentItem, FactItem, ObservationItem, ProcedureItem, RecallSlots, SummaryItem, WorkStateList,
};
use serde_json::Value;
use uuid::Uuid;

/// claude-mem `plugin/modes/code.json` observation type glyphs, in legend order.
pub const GLYPHS: &[(&str, &str)] = &[
    ("decision", "⚖"),
    ("bugfix", "●"),
    ("feature", "◆"),
    ("refactor", "↻"),
    ("change", "✓"),
    ("discovery", "○"),
    ("security_alert", "⚠"),
    ("security_note", "⚷"),
    ("sensitive", "⊘"),
];
pub const DEFAULT_GLYPH: &str = "•";
pub const INDEX_HEADER: &str = "| id | time | t | title | ~tok |\n|---|---|---|---|---|";
const EXCERPT_CHARS: usize = 160;
const NARRATIVE_CHARS: usize = 600;
const DOCUMENT_EXCERPT_CHARS: usize = 480;

/// `_Legend: ⚖ decision · ● bugfix · … · § summary_`.
pub fn legend() -> String {
    let items: Vec<String> = GLYPHS.iter().map(|(t, g)| format!("{g} {t}")).collect();
    format!("_Legend: {} · § summary_", items.join(" · "))
}

pub fn glyph(table: MemoryTable, obs_type: Option<&str>) -> &'static str {
    match table {
        MemoryTable::Observations => {
            GLYPHS.iter().find(|(t, _)| Some(*t) == obs_type).map_or(DEFAULT_GLYPH, |(_, g)| *g)
        }
        MemoryTable::Summaries => "§",
        MemoryTable::Facts => "≡",
        MemoryTable::Procedures => "¶",
        MemoryTable::Documents => "▤",
    }
}

pub fn fmt_date(dt: Option<DateTime<Utc>>) -> String {
    dt.map_or_else(|| "?".into(), |d| d.format("%Y-%m-%d").to_string())
}

pub fn fmt_md(dt: Option<DateTime<Utc>>) -> String {
    dt.map_or_else(|| "?".into(), |d| d.format("%m-%d").to_string())
}

pub fn fmt_when(dt: Option<DateTime<Utc>>) -> String {
    dt.map_or_else(|| "?".into(), |d| d.format("%m-%d %H:%M").to_string())
}

fn fmt_time(dt: DateTime<Utc>) -> String {
    dt.format("%H:%M").to_string()
}

/// Collapse whitespace runs to single spaces and trim.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// [`one_line`] cut to `limit` characters with a trailing `…`.
pub fn excerpt(text: &str, limit: usize) -> String {
    let s = one_line(text);
    if s.chars().count() <= limit {
        return s;
    }
    let cut: String = s.chars().take(limit.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

fn cell(text: &str) -> String {
    one_line(text).replace('|', "\\|")
}

// ---------- citation tags ----------

fn tag(kind: &str, id: &str, parts: &[Option<String>]) -> String {
    let mut bits = vec![format!("{kind}#{id}")];
    bits.extend(parts.iter().flatten().filter(|p| !p.is_empty()).cloned());
    format!("[{}]", bits.join(" · "))
}

/// `[fact#a91c2e]`, with the caution (`disputed`, `unconfirmed 4 months`) after the id when the fact is to be
/// treated with care. What else is known of it (confidence, devices, evidence) is in `memory_get` with `trace`.
pub fn fact_tag(id: &Uuid, caution: Option<&str>) -> String {
    tag("fact", &short_id(id), &[caution.map(String::from)])
}

/// `[obs#a1b2c3 · laptop-x1 · 2026-10-22]`.
pub fn obs_tag(id: &Uuid, device: Option<&str>, created_at: Option<DateTime<Utc>>) -> String {
    tag("obs", &short_id(id), &[device.map(String::from), Some(fmt_date(created_at))])
}

/// `[sum#a1b2c3]`: the summary line already says when and where.
pub fn summary_tag(id: &Uuid) -> String {
    tag("sum", &short_id(id), &[])
}

/// `[proc#a1b2c3]`, with `disputed` after the id for a procedure that failed more often than it worked.
pub fn procedure_tag(id: &Uuid, caution: Option<&str>) -> String {
    tag("proc", &short_id(id), &[caution.map(String::from)])
}

/// `[doc#a1b2c3 · source: docs/adr/0003.md#Decision · 2026-10-02]`: `source` is `uri#Heading > Subheading` and the
/// date is when the chunk was ingested.
pub fn document_tag(id: &Uuid, source: &str, ingested_at: Option<DateTime<Utc>>) -> String {
    tag("doc", &short_id(id), &[Some(format!("source: {source}")), Some(fmt_date(ingested_at))])
}

fn with_citation(line: String, citation: &Option<String>) -> String {
    match citation {
        Some(c) => format!("{line} {c}"),
        None => line,
    }
}

// ---------- slot lines ----------

pub fn fact_line(f: &FactItem) -> String {
    let subject = match f.subject_kind.as_deref() {
        None | Some("user") => String::new(),
        Some(_) => format!("{} ", f.subject),
    };
    with_citation(format!("- {subject}{}: {}", f.predicate, one_line(&f.value_text)), &f.item.citation)
}

pub fn procedure_line(p: &ProcedureItem) -> String {
    let usage = if p.success_count > 0 || p.last_used_at.is_some() {
        let last = p.last_used_at.map(|d| format!(", last {}", fmt_md(Some(d)))).unwrap_or_default();
        format!("used {}×{last}", p.success_count)
    } else {
        format!("v{}", p.version)
    };
    with_citation(
        format!("- **{}** ({usage}): {}", one_line(&p.name), excerpt(&p.body_md, EXCERPT_CHARS)),
        &p.item.citation,
    )
}

pub fn document_line(d: &DocumentItem) -> String {
    with_citation(format!("- {}", excerpt(&d.content, DOCUMENT_EXCERPT_CHARS)), &d.item.citation)
}

pub fn observation_block(o: &ObservationItem) -> String {
    let mut head = format!("- {} **{}**", glyph(MemoryTable::Observations, Some(&o.obs_type)), one_line(&o.title));
    if let Some(sub) = o.subtitle.as_deref().filter(|s| !s.trim().is_empty()) {
        head.push_str(&format!(" — {}", one_line(sub)));
    }
    let mut lines = vec![with_citation(head, &o.item.citation)];
    if let Some(n) = o.narrative.as_deref().filter(|n| !n.trim().is_empty()) {
        lines.push(format!("  {}", excerpt(n, NARRATIVE_CHARS)));
    }
    lines.extend(o.facts.iter().map(|f| format!("  - {}", one_line(f))));
    lines.join("\n")
}

pub fn summary_line(s: &SummaryItem) -> String {
    let who = s.device_id.as_deref().map(|d| format!(" {d}")).unwrap_or_default();
    let text = match s.request.as_deref().filter(|r| !r.trim().is_empty()) {
        Some(r) => format!("\"{}\"", excerpt(r, 120)),
        None => excerpt(s.learned.as_deref().or(s.completed.as_deref()).unwrap_or(""), 120),
    };
    let next = s
        .next_steps
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .map(|n| format!(" → next: {}", excerpt(n, 160)))
        .unwrap_or_default();
    with_citation(format!("- {}{who} · {text}{next}", fmt_date(Some(s.created_at))), &s.item.citation)
}

/// Short scope label: the last path segment of a project key, the key otherwise.
pub fn scope_label(scope: &Scope) -> String {
    match scope.kind {
        ScopeKind::Global => "global".into(),
        ScopeKind::Project => {
            let k = scope.key.trim_end_matches('/');
            k.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(&scope.key).to_string()
        }
        _ => scope.key.clone(),
    }
}

fn state_mark(value: &Value) -> String {
    match value {
        Value::Bool(true) => "✓".into(),
        Value::Bool(false) | Value::Null => "☐".into(),
        Value::String(s) => match one_line(s).to_lowercase().as_str() {
            "done" | "complete" | "completed" | "x" | "✓" | "true" | "yes" => "✓".into(),
            "todo" | "open" | "pending" | "" | "☐" | "false" | "no" | "wip" => "☐".into(),
            _ => excerpt(s, 40),
        },
        other => excerpt(&other.to_string(), 40),
    }
}

/// `- todo/memory: migrations ✓ · api deploy ☐`.
pub fn work_state_line(w: &WorkStateList) -> String {
    let items: Vec<String> = w.fields.iter().map(|(k, v)| format!("{} {}", one_line(k), state_mark(v))).collect();
    format!("- {}/{}: {}", w.list_name, scope_label(&w.scope), items.join(" · "))
}

// ---------- index ----------

/// One row of the progressive-disclosure index.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexEntry {
    pub id: Uuid,
    pub when: DateTime<Utc>,
    pub glyph: &'static str,
    pub title: String,
    pub tokens: i32,
    pub anchor: bool,
    /// `disputed` / `unconfirmed 4 months`, shown after the title.
    pub caution: Option<String>,
    /// Rows of the same memory said again, folded into this one (see [`collapse_similar`]).
    pub similar: usize,
}

impl IndexEntry {
    pub fn from_row(r: &IndexRow) -> Self {
        Self {
            id: r.id,
            when: r.created_at,
            glyph: glyph(r.table, r.row_type.as_deref()),
            title: r.title.clone(),
            tokens: r.token_estimate,
            anchor: r.is_anchor,
            caution: r.caution.clone(),
            similar: 0,
        }
    }

    pub fn from_observation(o: &ObservationItem) -> Self {
        Self {
            id: o.item.id,
            when: o.created_at,
            glyph: glyph(MemoryTable::Observations, Some(&o.obs_type)),
            title: o.title.clone(),
            tokens: o.item.token_estimate,
            anchor: false,
            caution: None,
            similar: 0,
        }
    }
}

/// What makes two rows the same memory said twice: table, type, scope and normalized title. A title that
/// normalizes to nothing matches nothing.
fn similar_key(table: MemoryTable, row_type: Option<&str>, scope: &Scope, title: &str) -> Option<String> {
    let norm = normalize_title(title);
    (!norm.is_empty()).then(|| format!("{}\0{}\0{}\0{norm}", table.as_str(), row_type.unwrap_or(""), scope.as_text()))
}

/// Fold rows with the same key into one: the newest, in the place of the first, counting the rest in `similar`.
fn collapse_similar(keyed: impl IntoIterator<Item = (Option<String>, IndexEntry)>) -> Vec<IndexEntry> {
    let mut out: Vec<IndexEntry> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (key, entry) in keyed {
        let Some(&i) = key.as_ref().and_then(|k| at.get(k)) else {
            if let Some(k) = key {
                at.insert(k, out.len());
            }
            out.push(entry);
            continue;
        };
        let similar = out[i].similar + 1;
        if entry.when > out[i].when {
            out[i] = entry;
        }
        out[i].similar = similar;
    }
    out
}

/// A row: `| 9f6f34 | 10:16 | ⚖ | title | 168 |`. `reference` is the id as it is to be shown.
pub fn index_line(e: &IndexEntry, reference: &str) -> String {
    let reference = if e.anchor { format!("**▶ {reference}**") } else { reference.to_string() };
    let mut title = cell(&e.title);
    if let Some(c) = &e.caution {
        title.push_str(&format!(" [{c}]"));
    }
    if e.similar > 0 {
        title.push_str(&format!(" (+{} similar)", e.similar));
    }
    format!("| {reference} | {} | {} | {title} | {} |", fmt_time(e.when), e.glyph, e.tokens)
}

/// Header, then the rows under one `**2026-10-22**` line per day (days in the order they first appear, rows in
/// the order given), with the shortest ids that tell the rows apart; empty for no entries.
pub fn index_table(entries: &[IndexEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let refs = short_ids(&entries.iter().map(|e| e.id).collect::<Vec<_>>());
    let mut days: Vec<(NaiveDate, Vec<usize>)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let day = e.when.date_naive();
        match days.iter_mut().find(|(d, _)| *d == day) {
            Some((_, rows)) => rows.push(i),
            None => days.push((day, vec![i])),
        }
    }
    let mut lines = vec![INDEX_HEADER.to_string()];
    for (day, rows) in days {
        lines.push(format!("**{day}**"));
        lines.extend(rows.into_iter().map(|i| index_line(&entries[i], &refs[i])));
    }
    lines.join("\n")
}

/// [`index_table`] and the glyph legend, for the blocks that start a session.
fn index_with_legend(entries: &[IndexEntry]) -> String {
    let table = index_table(entries);
    if table.is_empty() { table } else { format!("{table}\n{}", legend()) }
}

fn row_entries(rows: &[IndexRow], collapse: bool) -> Vec<IndexEntry> {
    if !collapse {
        return rows.iter().map(IndexEntry::from_row).collect();
    }
    collapse_similar(
        rows.iter().map(|r| (similar_key(r.table, r.row_type.as_deref(), &r.scope, &r.title), IndexEntry::from_row(r))),
    )
}

/// `memory_search` output: the rows, near-duplicates folded into one.
pub fn search_rows_markdown(rows: &[IndexRow]) -> String {
    if rows.is_empty() {
        return "_No matching memories._".into();
    }
    index_table(&row_entries(rows, true))
}

/// `memory_timeline` output: every row in time order, the anchor marked.
pub fn timeline_rows_markdown(rows: &[IndexRow]) -> String {
    if rows.is_empty() {
        return "_No matching memories._".into();
    }
    index_table(&row_entries(rows, false))
}

// ---------- sections ----------

pub fn section(title: &str, body: &str) -> String {
    if body.trim().is_empty() { String::new() } else { format!("### {title}\n{body}") }
}

fn join_sections(parts: &[String]) -> String {
    parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join("\n")
}

fn lines<T>(items: &[T], f: impl Fn(&T) -> String) -> String {
    items.iter().map(f).collect::<Vec<_>>().join("\n")
}

fn profile_body(slots: &RecallSlots) -> String {
    let mut out: Vec<String> = Vec::new();
    if let Some(p) = slots.profile.as_ref().filter(|p| !p.body_md.trim().is_empty()) {
        out.push(p.body_md.trim().to_string());
    }
    out.extend(slots.facts.iter().filter(|f| f.pinned).map(fact_line));
    out.join("\n")
}

/// `rendered_markdown` of `POST /v1/recall`.
pub fn recall_markdown(slots: &RecallSlots) -> String {
    let others: Vec<&FactItem> = slots.facts.iter().filter(|f| !f.pinned).collect();
    let expanded: Vec<&ObservationItem> = slots.observations.iter().filter(|o| o.expanded).collect();
    let rest = observation_entries(slots.observations.iter().filter(|o| !o.expanded));
    let mut obs_parts: Vec<String> = expanded.iter().map(|o| observation_block(o)).collect();
    if !rest.is_empty() {
        obs_parts.push(index_with_legend(&rest));
    }
    let body = join_sections(&[
        section("Profile", &profile_body(slots)),
        section("Facts", &lines(&others, |f| fact_line(f))),
        section("Open work", &lines(&slots.work_state, work_state_line)),
        section("Relevant procedures", &lines(&slots.procedures, procedure_line)),
        section("Observations", &obs_parts.join("\n")),
        section("Summaries", &lines(&slots.summaries, summary_line)),
        section("Documents", &lines(&slots.documents, document_line)),
    ]);
    if body.is_empty() { "## Memory\n_No relevant memories._".into() } else { format!("## Memory\n{body}") }
}

/// The index rows of observations, near-duplicates folded into one.
fn observation_entries<'a>(observations: impl Iterator<Item = &'a ObservationItem>) -> Vec<IndexEntry> {
    collapse_similar(observations.map(|o| {
        (
            similar_key(MemoryTable::Observations, Some(&o.obs_type), &o.item.scope, &o.title),
            IndexEntry::from_observation(o),
        )
    }))
}

pub const RECENT_OBSERVATIONS_TITLE: &str = "Recent observations (fetch with memory_get)";
const RECENT_SESSIONS_TITLE: &str = "Recent sessions";

/// Cached portion of the SessionStart block: everything except the live sections.
pub fn context_body(slots: &RecallSlots) -> String {
    let entries = observation_entries(slots.observations.iter());
    join_sections(&[
        section("Profile", &profile_body(slots)),
        section("Open work", &lines(&slots.work_state, work_state_line)),
        section("Relevant procedures", &lines(&slots.procedures, procedure_line)),
        section(RECENT_OBSERVATIONS_TITLE, &index_with_legend(&entries)),
        section(RECENT_SESSIONS_TITLE, &lines(&slots.summaries, summary_line)),
    ])
}

/// The cached block without the Recent sessions lines of summaries that since-you-were-here already lists.
pub fn without_listed_sessions(body: &str, listed: &[Uuid]) -> String {
    let marks: Vec<String> = listed.iter().map(summary_tag).collect();
    let recent = format!("### {RECENT_SESSIONS_TITLE}");
    let mut sections: Vec<Vec<&str>> = Vec::new();
    for line in body.lines() {
        if line.starts_with("### ") || sections.is_empty() {
            sections.push(Vec::new());
        }
        if let Some(last) = sections.last_mut() {
            last.push(line);
        }
    }
    for section in sections.iter_mut().filter(|s| s[0] == recent) {
        section.retain(|line| *line == recent || !marks.iter().any(|m| line.contains(m)));
    }
    sections.retain(|s| s.as_slice() != [recent.as_str()]);
    sections.concat().join("\n")
}

/// What the lean block tells the model about the pull tools (their arguments are in the tool definitions).
pub const LEAN_RUBRIC: &str = "### Memory tools
Only the profile and open work are loaded here; the rest of the shared memory is on demand.
- `memory_search`: when the task may depend on earlier sessions, decisions or fixes, or the user refers to them. Returns an index of ids and titles.
- `memory_timeline`: the rows around a search hit.
- `memory_get`: full bodies, only for the ids you need.
- `memory_remember`: only when the user asks you to keep something.
Do not search for what the task already tells you.";

/// Cached portion of the lean SessionStart block: profile, open work and [`LEAN_RUBRIC`].
pub fn lean_body(slots: &RecallSlots) -> String {
    join_sections(&[
        section("Profile", &profile_body(slots)),
        section("Open work", &lines(&slots.work_state, work_state_line)),
        LEAN_RUBRIC.to_string(),
    ])
}

/// One session in since-you-were-here (11.7).
#[derive(Debug, Clone, PartialEq)]
pub struct SinceRow {
    /// The summary the line is made from, when the session has one.
    pub summary_id: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub device: String,
    pub request: Option<String>,
    pub next_steps: Option<String>,
}

/// `### Since you were here (laptop-x1, last 2026-10-20)` and one line per session elsewhere.
pub fn since_section(device: &str, last: Option<DateTime<Utc>>, rows: &[SinceRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let last = last.map_or_else(|| "none in 14 days".to_string(), |d| fmt_date(Some(d)));
    let body = lines(rows, |r| {
        let what = match r.request.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(q) => format!("\"{}\"", excerpt(q, 120)),
            None => "(no summary yet)".into(),
        };
        let next = r
            .next_steps
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|n| format!(" → next: {}", excerpt(n, 160)))
            .unwrap_or_default();
        format!("- {} {} · {what}{next}", fmt_date(Some(r.started_at)), r.device)
    });
    section(&format!("Since you were here ({device}, last {last})"), &body)
}

/// `## Memory`, live banners, the cached Profile, live since-you-were-here, then the rest of the
/// cached sections.
pub fn assemble_context(banners: &[String], cached_body: &str, since: &str) -> String {
    let mut body = cached_body.to_string();
    if !since.is_empty() {
        body = match body.strip_prefix("### Profile\n") {
            Some(after) => match after.find("\n### ") {
                Some(i) => {
                    let split = "### Profile\n".len() + i;
                    format!("{}\n{since}{}", &body[..split], &body[split..])
                }
                None => format!("{body}\n{since}"),
            },
            None if body.is_empty() => since.to_string(),
            None => format!("{since}\n{body}"),
        };
    }
    let mut out = String::from("## Memory\n");
    for b in banners {
        out.push_str(&format!("> memory: {b}\n"));
    }
    out.push_str(if body.is_empty() { "_No memories for this project yet._" } else { &body });
    out
}

// ---------- memory bodies (memory_get) and trace ----------

/// `#### ⚖ Title [obs#d94f56 · decision · billing-api · 10-24 10:16]`: one line, the tag carrying what is known of the
/// row (a project scope by the last segment of its key, an active fact or procedure without its status).
fn item_header(glyph: &str, title: &str, kind: &str, id: &str, parts: Vec<Option<String>>) -> String {
    format!("#### {glyph} {title} {}", tag(kind, id, &parts))
}

fn item_markdown(item: &MemoryItem, id: &str) -> String {
    match item {
        MemoryItem::Observations(o) => {
            let header = item_header(
                glyph(MemoryTable::Observations, Some(&o.obs_type)),
                &one_line(&o.title),
                "obs",
                id,
                vec![Some(o.obs_type.clone()), Some(scope_label(&o.scope)), Some(fmt_when(Some(o.created_at)))],
            );
            let mut lines = vec![header];
            if let Some(s) = o.subtitle.as_deref().filter(|s| !s.trim().is_empty()) {
                lines.push(format!("_{}_", one_line(s)));
            }
            if let Some(n) = o.narrative.as_deref().filter(|n| !n.trim().is_empty()) {
                lines.push(n.trim().to_string());
            }
            lines.extend(o.facts.iter().map(|f| format!("- {}", one_line(f))));
            if !o.files.is_empty() {
                lines.push(format!("Files: {}", o.files.join(", ")));
            }
            lines.join("\n")
        }
        MemoryItem::Summaries(s) => {
            let header = item_header(
                "§",
                &format!("{} summary", s.grain),
                "sum",
                id,
                vec![Some(scope_label(&s.scope)), Some(fmt_when(Some(s.created_at)))],
            );
            let mut lines = vec![header];
            for (label, value) in [
                ("request", &s.request),
                ("investigated", &s.investigated),
                ("learned", &s.learned),
                ("completed", &s.completed),
                ("next_steps", &s.next_steps),
                ("notes", &s.notes),
            ] {
                if let Some(v) = value.as_deref().filter(|v| !v.trim().is_empty()) {
                    lines.push(format!("**{label}**: {}", v.trim()));
                }
            }
            lines.join("\n")
        }
        MemoryItem::Facts(f) => [
            item_header(
                "≡",
                &format!("{} {}", f.subject.canonical_name, f.predicate),
                "fact",
                id,
                vec![
                    Some(f.status.to_string()).filter(|status| status != "active"),
                    Some(scope_label(&f.scope)),
                    Some(format!("confidence {:.2}", f.confidence)),
                    Some(format!("confirmations {}", f.confirmations)),
                    f.caution.clone(),
                ],
            ),
            one_line(&f.value_text),
        ]
        .join("\n"),
        MemoryItem::Procedures(p) => [
            item_header(
                "¶",
                &one_line(&p.name),
                "proc",
                id,
                vec![
                    Some(p.task_class.clone()),
                    Some(format!("v{}", p.version)),
                    serde_json::to_value(p.status)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .filter(|status| status != "active"),
                    Some(scope_label(&p.scope)),
                    procedure_caution(p.success_count, p.failure_count),
                ],
            ),
            format!("When: {}", one_line(&p.trigger_text)),
            p.body_md.trim().to_string(),
        ]
        .join("\n"),
        MemoryItem::Documents(d) => document_markdown(d, id),
    }
}

fn document_markdown(d: &DocumentFull, id: &str) -> String {
    let retired = d.retired_at.map(|at| format!("retired {}", fmt_when(Some(at))));
    [
        item_header(
            glyph(MemoryTable::Documents, None),
            &one_line(&d.title),
            "doc",
            id,
            vec![
                Some(format!("source: {}", d.source())),
                Some(scope_label(&d.scope)),
                Some(format!("chunk {}", d.chunk_index)),
                Some(format!("ingested {}", fmt_when(Some(d.created_at)))),
                retired,
            ],
        ),
        d.content.trim().to_string(),
    ]
    .join("\n")
}

/// One body per item, each headed by one line with the shortest id that tells the items apart.
pub fn memory_item_markdowns(items: &[MemoryItem]) -> Vec<String> {
    let refs = short_ids(&items.iter().map(MemoryItem::id).collect::<Vec<_>>());
    items.iter().zip(&refs).map(|(item, id)| item_markdown(item, id)).collect()
}

pub fn memory_items_markdown(items: &[MemoryItem]) -> String {
    if items.is_empty() {
        return "_Nothing found._".into();
    }
    memory_item_markdowns(items).join("\n\n")
}

fn provenance_lines(p: &Provenance, indent: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(s) = &p.session {
        out.push(format!(
            "{indent}- session {} · {} · {} · {}",
            s.harness,
            s.project_key.as_deref().unwrap_or("-"),
            s.device_id.as_deref().unwrap_or("-"),
            fmt_when(Some(s.started_at))
        ));
    }
    if !p.event_seqs.is_empty() {
        let seqs: Vec<String> = p.event_seqs.iter().map(i32::to_string).collect();
        out.push(format!("{indent}- events seq {}", seqs.join(", ")));
    }
    if let Some(obj) = p.extracted_by.as_object().filter(|o| !o.is_empty()) {
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        let parts: Vec<String> = keys
            .into_iter()
            .map(|k| match &obj[k] {
                Value::String(s) => format!("{k}={s}"),
                v => format!("{k}={v}"),
            })
            .collect();
        out.push(format!("{indent}- extracted by {}", parts.join(", ")));
    }
    out
}

/// Where a memory came from and how it changed, to follow its body in `memory_get` with `trace`: the provenance
/// (session, events, extractor, the evidence observations), the audit trail and, for a fact or procedure that
/// was replaced or replaced another, the supersession chain, oldest first.
pub fn trace_markdown(e: &ExplainResponse, h: &HistoryResponse) -> String {
    let mut lines = vec!["**Provenance**".to_string()];
    let prov = e.provenance.as_ref().map(|p| provenance_lines(p, "")).unwrap_or_default();
    if prov.is_empty() && e.evidence.is_empty() {
        lines.push("- (none recorded)".into());
    }
    lines.extend(prov);
    for ev in &e.evidence {
        lines.push(format!(
            "- evidence {} {} ({})",
            obs_tag(&ev.id, None, Some(ev.created_at)),
            one_line(&ev.title),
            ev.obs_type
        ));
        lines.extend(provenance_lines(&ev.provenance, "  "));
    }
    if !e.audit.is_empty() {
        lines.push("**Audit**".into());
        for a in &e.audit {
            let reason = a.reason.as_deref().map(|r| format!(": {}", one_line(r))).unwrap_or_default();
            lines.push(format!("- {} {} by {}{reason}", fmt_when(Some(a.created_at)), a.action, a.actor));
        }
    }
    if h.entries.len() > 1 {
        lines.push(format!("**History** ({} versions)", h.entries.len()));
        let refs = short_ids(&h.entries.iter().map(|e| e.id).collect::<Vec<_>>());
        for (e, id) in h.entries.iter().zip(&refs) {
            let until = e.valid_to.map_or_else(|| "now".into(), |t| fmt_date(Some(t)));
            let mark = if e.current { " ← asked" } else { "" };
            lines.push(format!(
                "- {} `{id}` {} · {} → {} · {}{mark}",
                glyph(e.table, None),
                e.status,
                fmt_date(e.valid_from),
                until,
                excerpt(&e.text, EXCERPT_CHARS)
            ));
            if let Some(actor) = &e.ended_by {
                let reason =
                    e.reason.as_deref().map(|r| format!(": {}", excerpt(r, EXCERPT_CHARS))).unwrap_or_default();
                let model = e.model.as_deref().map(|m| format!(" ({m})")).unwrap_or_default();
                lines.push(format!("  ended {} by {actor}{model}{reason}", fmt_when(e.ended_at)));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use memvault_core::schemas::common::ContentType;
    use memvault_core::schemas::memories::HistoryEntry;
    use memvault_core::schemas::recall::SlotItem;

    fn item(n: u128) -> SlotItem {
        SlotItem {
            id: Uuid::from_u128(n),
            space_id: Uuid::nil(),
            scope: Scope::global(),
            score: 0.0,
            token_estimate: 12,
            citation: None,
        }
    }

    fn observation(n: u128, title: &str, obs_type: &str, when: &str) -> ObservationItem {
        ObservationItem {
            item: item(n),
            obs_type: obs_type.into(),
            title: title.into(),
            subtitle: None,
            narrative: None,
            facts: vec![],
            files: vec![],
            importance: 0.5,
            created_at: when.parse().unwrap(),
            session_id: None,
            device_id: None,
            expanded: false,
        }
    }

    fn row(n: u128, table: MemoryTable, row_type: &str, title: &str, when: &str) -> IndexRow {
        IndexRow {
            id: Uuid::from_u128(n),
            table,
            row_type: Some(row_type.into()),
            title: title.into(),
            created_at: when.parse().unwrap(),
            scope: Scope::global(),
            token_estimate: 12,
            score: None,
            is_anchor: false,
            caution: None,
            importance: None,
        }
    }

    #[test]
    fn tags_and_excerpts() {
        let id = Uuid::from_u128(0xa91c2e);
        let day = "2026-10-02T08:00:00Z".parse().ok();
        assert_eq!(fact_tag(&id, None), "[fact#a91c2e]");
        assert_eq!(fact_tag(&id, Some("unconfirmed 4 months")), "[fact#a91c2e · unconfirmed 4 months]");
        assert_eq!(procedure_tag(&id, Some("disputed")), "[proc#a91c2e · disputed]");
        assert_eq!(summary_tag(&id), "[sum#a91c2e]");
        assert_eq!(obs_tag(&id, None, day), "[obs#a91c2e · 2026-10-02]");
        assert_eq!(excerpt("  a  b\nc ", 10), "a b c");
        assert_eq!(excerpt("abcdefghij", 5), "abcd…");
        assert_eq!(scope_label(&Scope::new(ScopeKind::Project, "github.com/alex/homelab")), "homelab");
    }

    #[test]
    fn index_names_each_day_once_and_shows_only_the_time() {
        let o = observation(0xa1b2c3, "Chose pgvector | not Chroma", "decision", "2026-10-22T14:03:00Z");
        let table = index_table(&[IndexEntry::from_observation(&o)]);
        assert_eq!(
            table,
            format!("{INDEX_HEADER}\n**2026-10-22**\n| a1b2c3 | 14:03 | ⚖ | Chose pgvector \\| not Chroma | 12 |")
        );
        assert_eq!(index_table(&[]), "");

        let entries: Vec<IndexEntry> = [
            observation(1, "one", "bugfix", "2026-10-22T14:03:00Z"),
            observation(2, "two", "bugfix", "2026-10-21T09:00:00Z"),
            observation(3, "three", "bugfix", "2026-10-22T08:30:00Z"),
        ]
        .iter()
        .map(IndexEntry::from_observation)
        .collect();
        let lines: Vec<String> = index_table(&entries).lines().skip(2).map(String::from).collect();
        let days: Vec<&str> = lines.iter().filter(|l| l.starts_with("**")).map(String::as_str).collect();
        assert_eq!(days, ["**2026-10-22**", "**2026-10-21**"], "days in the order they first appear");
        assert_eq!(lines.len(), 5);
        assert!(lines[1].contains("one") && lines[2].contains("three") && lines[4].contains("two"), "{lines:?}");
    }

    #[test]
    fn legend_closes_the_session_start_index_only() {
        let o = observation(0xa1b2c3, "Chose pgvector", "decision", "2026-10-22T14:03:00Z");
        let entries = [IndexEntry::from_observation(&o)];
        assert_eq!(index_with_legend(&entries), format!("{}\n{}", index_table(&entries), legend()));
        assert!(legend().starts_with("_Legend: ⚖ decision · ● bugfix · ◆ feature"));
        assert_eq!(index_with_legend(&[]), "");
        let rows = [row(0xa1b2c3, MemoryTable::Observations, "decision", "Chose pgvector", "2026-10-22T14:03:00Z")];
        assert!(!search_rows_markdown(&rows).contains("Legend") && !timeline_rows_markdown(&rows).contains("Legend"));
        assert_eq!(search_rows_markdown(&[]), "_No matching memories._");
    }

    #[test]
    fn ids_lengthen_only_to_tell_the_rows_shown_apart() {
        let rows = [
            row(0x1a1b2c3, MemoryTable::Observations, "bugfix", "first", "2026-10-22T14:03:00Z"),
            row(0x2a1b2c3, MemoryTable::Observations, "bugfix", "second", "2026-10-22T14:04:00Z"),
            row(0xffffff, MemoryTable::Observations, "bugfix", "third", "2026-10-22T14:05:00Z"),
        ];
        let table = timeline_rows_markdown(&rows);
        assert!(
            table.contains("| 1a1b2c3 |") && table.contains("| 2a1b2c3 |") && table.contains("| ffffff |"),
            "{table}"
        );
    }

    #[test]
    fn near_duplicates_fold_into_the_newest_unless_they_differ_in_kind() {
        let rows = [
            row(1, MemoryTable::Observations, "bugfix", "Fixed the bug", "2026-10-20T10:00:00Z"),
            row(2, MemoryTable::Observations, "bugfix", "fixed: THE bug.", "2026-10-22T10:00:00Z"),
            row(3, MemoryTable::Observations, "decision", "Fixed the bug", "2026-10-21T10:00:00Z"),
            row(4, MemoryTable::Facts, "bugfix", "Fixed the bug", "2026-10-21T11:00:00Z"),
            row(5, MemoryTable::Observations, "bugfix", "Fixed the bug", "2026-10-19T10:00:00Z"),
            row(6, MemoryTable::Observations, "bugfix", "!!", "2026-10-18T10:00:00Z"),
            row(7, MemoryTable::Observations, "bugfix", "??", "2026-10-17T10:00:00Z"),
        ];
        let table = search_rows_markdown(&rows);
        assert_eq!(table.matches("(+2 similar)").count(), 1, "{table}");
        assert!(
            table.contains("| 000002 | 10:00 | ● | fixed: THE bug. (+2 similar) |"),
            "the newest stands for the rest: {table}"
        );
        for kept in ["000003", "000004", "000006", "000007"] {
            assert!(table.contains(&format!("| {kept} |")), "{kept}: {table}");
        }
        assert!(!table.contains("| 000001 |") && !table.contains("| 000005 |"), "{table}");
        let all = timeline_rows_markdown(&rows);
        assert!(!all.contains("similar") && all.contains("| 000001 |"), "the timeline keeps every row: {all}");

        let mut scoped = row(8, MemoryTable::Observations, "bugfix", "Fixed the bug", "2026-10-22T09:00:00Z");
        scoped.scope = Scope::new(ScopeKind::Project, "github.com/a/b");
        assert!(
            !search_rows_markdown(&[rows[1].clone(), scoped]).contains("similar"),
            "another scope is another memory"
        );
    }

    #[test]
    fn session_start_index_folds_near_duplicates_too() {
        let observations = [
            observation(1, "Staging URL logged", "bugfix", "2026-10-22T10:00:00Z"),
            observation(2, "staging url logged!", "bugfix", "2026-10-21T10:00:00Z"),
        ];
        let slots = RecallSlots { observations: observations.to_vec(), ..RecallSlots::default() };
        let body = context_body(&slots);
        assert!(body.contains("| 000001 | 10:00 | ● | Staging URL logged (+1 similar) |"), "{body}");
        assert!(!body.contains("000002"), "{body}");
    }

    #[test]
    fn listed_sessions_are_not_repeated_in_recent_sessions() {
        let (a, b) = (Uuid::from_u128(0xaaaaaa), Uuid::from_u128(0xbbbbbb));
        let body = "### Profile\n- p\n### Recent sessions\n- 10-23 vm · \"one\" [sum#aaaaaa]\n- 10-22 vm · \"two\" [sum#bbbbbb]\n### Open work\n- w";
        assert_eq!(without_listed_sessions(body, &[]), body);
        assert_eq!(
            without_listed_sessions(body, &[a]),
            "### Profile\n- p\n### Recent sessions\n- 10-22 vm · \"two\" [sum#bbbbbb]\n### Open work\n- w"
        );
        assert_eq!(without_listed_sessions(body, &[a, b]), "### Profile\n- p\n### Open work\n- w");
        assert_eq!(without_listed_sessions("", &[a]), "");
    }

    #[test]
    fn since_goes_after_profile() {
        let since = "### Since you were here (x, last 2026-10-20)\n- a";
        assert_eq!(
            assemble_context(&[], "### Profile\n- p\n### Open work\n- w", since),
            format!("## Memory\n### Profile\n- p\n{since}\n### Open work\n- w")
        );
        assert_eq!(
            assemble_context(&[], "### Open work\n- w", since),
            format!("## Memory\n{since}\n### Open work\n- w")
        );
        assert_eq!(
            assemble_context(&["2 packets waiting".into()], "", ""),
            "## Memory\n> memory: 2 packets waiting\n_No memories for this project yet._"
        );
    }

    #[test]
    fn trace_lists_versions_oldest_first_and_only_for_a_chain() {
        let entry = |n: u128, status: &str, text: &str, to: Option<&str>| HistoryEntry {
            id: Uuid::from_u128(n),
            table: MemoryTable::Facts,
            text: text.into(),
            status: status.into(),
            valid_from: "2026-01-01T00:00:00Z".parse().ok(),
            valid_to: to.and_then(|t| t.parse().ok()),
            supersedes_id: None,
            superseded_by: None,
            current: n == 2,
            ended_at: to.and_then(|t| t.parse().ok()),
            ended_by: to.map(|_| "planner".into()),
            reason: to.map(|_| "switched\neditors".into()),
            model: None,
            run_id: None,
        };
        let history = |entries| HistoryResponse { id: Uuid::from_u128(2), table: MemoryTable::Facts, entries };
        let explained = ExplainResponse {
            id: Uuid::from_u128(2),
            table: MemoryTable::Facts,
            item: MemoryItem::Documents(DocumentFull {
                id: Uuid::from_u128(2),
                space_id: Uuid::nil(),
                scope: Scope::global(),
                source_uri: "docs/a.md".into(),
                title: "A".into(),
                content_type: ContentType::Markdown,
                chunk_index: 0,
                heading_path: vec![],
                content: "text".into(),
                content_hash: "h".into(),
                token_estimate: 1,
                ingested_by: None,
                created_at: "2026-01-01T00:00:00Z".parse().unwrap(),
                retired_at: None,
            }),
            provenance: None,
            evidence: vec![],
            audit: vec![],
        };
        let chain = history(vec![
            entry(1, "superseded", "prefers.editor: vim", Some("2026-03-01T00:00:00Z")),
            entry(2, "active", "prefers.editor: helix", None),
        ]);
        assert_eq!(
            trace_markdown(&explained, &chain),
            "**Provenance**\n- (none recorded)\n\
             **History** (2 versions)\n\
             - ≡ `000001` superseded · 2026-01-01 → 2026-03-01 · prefers.editor: vim\n  \
             ended 03-01 00:00 by planner: switched editors\n\
             - ≡ `000002` active · 2026-01-01 → now · prefers.editor: helix ← asked"
        );
        let alone = history(vec![entry(2, "active", "prefers.editor: helix", None)]);
        assert_eq!(trace_markdown(&explained, &alone), "**Provenance**\n- (none recorded)");
    }

    #[test]
    fn work_state_marks() {
        let mut fields = serde_json::Map::new();
        fields.insert("migrations".into(), Value::String("done".into()));
        fields.insert("api deploy".into(), Value::Bool(false));
        let w = WorkStateList {
            scope: Scope::new(ScopeKind::Project, "github.com/a/memory"),
            list_name: "todo".into(),
            fields,
            updated_at: None,
            token_estimate: 0,
        };
        assert_eq!(work_state_line(&w), "- todo/memory: api deploy ☐ · migrations ✓");
    }

    #[test]
    fn lean_body_is_profile_open_work_and_rubric() {
        assert_eq!(lean_body(&RecallSlots::default()), LEAN_RUBRIC);
        assert!(memvault_core::estimate_tokens(LEAN_RUBRIC) <= 200, "the rubric is part of a ~400 token block");
        for tool in ["memory_search", "memory_timeline", "memory_get", "memory_remember"] {
            assert!(LEAN_RUBRIC.contains(&format!("`{tool}`")), "{tool}");
        }
        let mut fields = serde_json::Map::new();
        fields.insert("migrations".into(), Value::String("done".into()));
        let work = WorkStateList {
            scope: Scope::new(ScopeKind::Project, "github.com/a/memory"),
            list_name: "todo".into(),
            fields,
            updated_at: None,
            token_estimate: 0,
        };
        let slots = RecallSlots { work_state: vec![work], ..RecallSlots::default() };
        assert_eq!(lean_body(&slots), format!("### Open work\n- todo/memory: migrations ✓\n{LEAN_RUBRIC}"));
    }
}
