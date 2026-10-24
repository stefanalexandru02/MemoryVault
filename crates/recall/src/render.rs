//! Deterministic markdown rendering (spec 11.5): citation tags, slot sections, the
//! progressive-disclosure index (claude-mem's code-mode glyphs), since-you-were-here, memory bodies
//! and explain chains.
//!
//! Everything here is a pure function of its inputs (UTC dates, stable ordering), so the same slots
//! always render to the same bytes. The MCP tools reuse [`index_rows_markdown`],
//! [`memory_items_markdown`] and [`explain_markdown`] so pull-mode output matches the SessionStart
//! block.

use chrono::{DateTime, Utc};
use memvault_core::ids::short_id;
use memvault_core::schemas::common::{MemoryTable, Scope, ScopeKind};
use memvault_core::schemas::memories::{ExplainResponse, IndexRow, MemoryItem, Provenance};
use memvault_core::schemas::recall::{
    FactItem, ObservationItem, ProcedureItem, RecallSlots, SummaryItem, WorkStateList,
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
pub const INDEX_HEADER: &str = "| id | when | t | title | ~tok |\n|---|---|---|---|---|";
const EXCERPT_CHARS: usize = 160;
const NARRATIVE_CHARS: usize = 600;

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

fn tag(kind: &str, id: &Uuid, parts: &[Option<String>]) -> String {
    let mut bits = vec![format!("{kind}#{}", short_id(id))];
    bits.extend(parts.iter().flatten().filter(|p| !p.is_empty()).cloned());
    format!("[{}]", bits.join(" · "))
}

/// `[fact#a91c2e · 0.83 · laptop+desktop · 2026-10-02]`.
pub fn fact_tag(id: &Uuid, confidence: f64, devices: &[String], first_seen: Option<DateTime<Utc>>) -> String {
    tag("fact", id, &[Some(format!("{confidence:.2}")), Some(devices.join("+")), Some(fmt_date(first_seen))])
}

/// `[obs#a1b2c3 · laptop-x1 · 2026-10-22]`.
pub fn obs_tag(id: &Uuid, device: Option<&str>, created_at: Option<DateTime<Utc>>) -> String {
    tag("obs", id, &[device.map(String::from), Some(fmt_date(created_at))])
}

pub fn summary_tag(id: &Uuid, device: Option<&str>, created_at: Option<DateTime<Utc>>) -> String {
    tag("sum", id, &[device.map(String::from), Some(fmt_date(created_at))])
}

pub fn procedure_tag(id: &Uuid, version: i32, created_at: Option<DateTime<Utc>>) -> String {
    tag("proc", id, &[Some(format!("v{version}")), Some(fmt_date(created_at))])
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
        }
    }
}

pub fn index_line(e: &IndexEntry, full_ids: bool) -> String {
    let mut reference = if full_ids { e.id.to_string() } else { short_id(&e.id) };
    if e.anchor {
        reference = format!("**▶ {reference}**");
    }
    format!("| {reference} | {} | {} | {} | {} |", fmt_when(Some(e.when)), e.glyph, cell(&e.title), e.tokens)
}

/// Header, one line per entry and the legend; empty for no entries.
pub fn index_table(entries: &[IndexEntry], full_ids: bool) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut lines = vec![INDEX_HEADER.to_string()];
    lines.extend(entries.iter().map(|e| index_line(e, full_ids)));
    lines.push(legend());
    lines.join("\n")
}

/// Search/timeline/by-file output for the MCP tools (full ids so `memory_get` can take them).
pub fn index_rows_markdown(rows: &[IndexRow]) -> String {
    if rows.is_empty() {
        return "_No matching memories._".into();
    }
    index_table(&rows.iter().map(IndexEntry::from_row).collect::<Vec<_>>(), true)
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
    let rest: Vec<IndexEntry> =
        slots.observations.iter().filter(|o| !o.expanded).map(IndexEntry::from_observation).collect();
    let mut obs_parts: Vec<String> = expanded.iter().map(|o| observation_block(o)).collect();
    if !rest.is_empty() {
        obs_parts.push(index_table(&rest, false));
    }
    let body = join_sections(&[
        section("Profile", &profile_body(slots)),
        section("Facts", &lines(&others, |f| fact_line(f))),
        section("Open work", &lines(&slots.work_state, work_state_line)),
        section("Relevant procedures", &lines(&slots.procedures, procedure_line)),
        section("Observations", &obs_parts.join("\n")),
        section("Summaries", &lines(&slots.summaries, summary_line)),
    ]);
    if body.is_empty() { "## Memory\n_No relevant memories._".into() } else { format!("## Memory\n{body}") }
}

pub const RECENT_OBSERVATIONS_TITLE: &str = "Recent observations (fetch with memory_get)";

/// Cached portion of the SessionStart block: everything except the live sections.
pub fn context_body(slots: &RecallSlots) -> String {
    let entries: Vec<IndexEntry> = slots.observations.iter().map(IndexEntry::from_observation).collect();
    join_sections(&[
        section("Profile", &profile_body(slots)),
        section("Open work", &lines(&slots.work_state, work_state_line)),
        section("Relevant procedures", &lines(&slots.procedures, procedure_line)),
        section(RECENT_OBSERVATIONS_TITLE, &index_table(&entries, false)),
        section("Recent sessions", &lines(&slots.summaries, summary_line)),
    ])
}

/// One session in since-you-were-here (11.7).
#[derive(Debug, Clone, PartialEq)]
pub struct SinceRow {
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

// ---------- memory bodies (memory_get) and explain ----------

pub fn memory_item_markdown(item: &MemoryItem) -> String {
    match item {
        MemoryItem::Observations(o) => {
            let mut lines = vec![
                format!(
                    "#### {} {} {}",
                    glyph(MemoryTable::Observations, Some(&o.obs_type)),
                    one_line(&o.title),
                    obs_tag(&o.id, None, Some(o.created_at))
                ),
                format!("`{}` · {} · {} · {}", o.id, o.obs_type, o.scope.as_text(), fmt_when(Some(o.created_at))),
            ];
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
            let mut lines = vec![
                format!("#### § {} summary {}", s.grain, summary_tag(&s.id, None, Some(s.created_at))),
                format!("`{}` · {} · {}", s.id, s.scope.as_text(), fmt_when(Some(s.created_at))),
            ];
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
            format!(
                "#### ≡ {} {} {}",
                f.subject.canonical_name,
                f.predicate,
                fact_tag(&f.id, f.confidence, &[], Some(f.first_seen_at))
            ),
            format!("`{}` · {} · {} · confirmations {}", f.id, f.status, f.scope.as_text(), f.confirmations),
            one_line(&f.value_text),
        ]
        .join("\n"),
        MemoryItem::Procedures(p) => [
            format!("#### ¶ {} {}", one_line(&p.name), procedure_tag(&p.id, p.version, Some(p.created_at))),
            format!(
                "`{}` · {} · {} · {}",
                p.id,
                p.task_class,
                serde_json::to_value(p.status).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
                p.scope.as_text()
            ),
            format!("When: {}", one_line(&p.trigger_text)),
            p.body_md.trim().to_string(),
        ]
        .join("\n"),
    }
}

pub fn memory_items_markdown(items: &[MemoryItem]) -> String {
    if items.is_empty() {
        return "_Nothing found._".into();
    }
    items.iter().map(memory_item_markdown).collect::<Vec<_>>().join("\n\n")
}

fn provenance_lines(p: &Provenance, indent: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(s) = &p.session {
        out.push(format!(
            "{indent}- session `{}` · {} · {} · {} · {}",
            s.id,
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

pub fn explain_markdown(e: &ExplainResponse) -> String {
    let mut lines = vec![memory_item_markdown(&e.item), String::new(), "**Provenance**".into()];
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
        lines.push(String::new());
        lines.push("**Audit**".into());
        for a in &e.audit {
            let reason = a.reason.as_deref().map(|r| format!(": {}", one_line(r))).unwrap_or_default();
            lines.push(format!("- {} {} by {}{reason}", fmt_when(Some(a.created_at)), a.action, a.actor));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn tags_and_excerpts() {
        let id = Uuid::from_u128(0xa91c2e);
        let day = "2026-10-02T08:00:00Z".parse().ok();
        assert_eq!(
            fact_tag(&id, 0.83, &["laptop".into(), "desktop".into()], day),
            "[fact#a91c2e · 0.83 · laptop+desktop · 2026-10-02]"
        );
        assert_eq!(obs_tag(&id, None, day), "[obs#a91c2e · 2026-10-02]");
        assert_eq!(excerpt("  a  b\nc ", 10), "a b c");
        assert_eq!(excerpt("abcdefghij", 5), "abcd…");
        assert_eq!(scope_label(&Scope::new(ScopeKind::Project, "github.com/alex/homelab")), "homelab");
    }

    #[test]
    fn index_and_legend() {
        let o = ObservationItem {
            item: item(0xa1b2c3),
            obs_type: "decision".into(),
            title: "Chose pgvector | not Chroma".into(),
            subtitle: None,
            narrative: None,
            facts: vec![],
            files: vec![],
            importance: 0.5,
            created_at: "2026-10-22T14:03:00Z".parse().unwrap(),
            session_id: None,
            device_id: None,
            expanded: false,
        };
        let table = index_table(&[IndexEntry::from_observation(&o)], false);
        assert_eq!(
            table,
            format!("{INDEX_HEADER}\n| a1b2c3 | 10-22 14:03 | ⚖ | Chose pgvector \\| not Chroma | 12 |\n{}", legend())
        );
        assert!(legend().starts_with("_Legend: ⚖ decision · ● bugfix · ◆ feature"));
        assert_eq!(index_table(&[], false), "");
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
}
