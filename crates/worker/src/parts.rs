//! Embedding parts per owner row (spec 13): which texts of a row get a vector, and how they are
//! named in `embeddings.part`.

use memvault_core::schemas::common::EmbeddingOwnerTable;
use serde_json::Value;

/// Smallest window a document chunk is cut into, so a tiny `EMBED_MAX_PART_TOKENS` still makes progress.
const MIN_WINDOW_CHARS: usize = 64;

/// One text to embed: `embeddings.part` and the (already truncated) text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub text: String,
}

/// Cut `text` to its first `max_tokens` tokens (token = 4 chars, the `estimate_tokens` convention).
pub fn truncate_tokens(text: &str, max_tokens: usize) -> &str {
    let max_chars = max_tokens.saturating_mul(4);
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

fn text_field<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

/// Parts of a document chunk: its text in windows of at most `max_tokens`, each led by the heading path so a
/// window from the middle of a section still says where it is. A retired chunk has none.
fn document_parts(row: &Value, max_tokens: usize) -> Vec<Part> {
    if row.get("retired_at").is_some_and(|v| !v.is_null()) {
        return Vec::new();
    }
    let content = text_field(row, "content").unwrap_or_default().trim();
    let headings: Vec<&str> =
        row.get("heading_path").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let lead = if headings.is_empty() { String::new() } else { format!("{}\n", headings.join(" > ")) };
    let window = max_tokens.saturating_mul(4).saturating_sub(lead.chars().count()).max(MIN_WINDOW_CHARS);
    let starts: Vec<usize> = content.char_indices().map(|(i, _)| i).step_by(window).collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, start)| {
            let end = starts.get(n + 1).copied().unwrap_or(content.len());
            Part { name: format!("body:{n}"), text: format!("{lead}{}", &content[*start..end]) }
        })
        .collect()
}

/// Parts of a row given as its `to_jsonb` form. Empty or whitespace-only texts are skipped;
/// `fact:i` keeps the index of the fact in the observation's `facts` array.
///
/// observations: `title`, `narrative`, `fact:i`; summaries: `request`, `learned`, `next_steps`;
/// facts: `value` (from `value_text`); procedures: `trigger` (from `trigger_text`), `body` (from
/// `body_md`); entities: `name` (from `canonical_name`); documents: `body:i`, one per window of the chunk.
pub fn parts_for(table: EmbeddingOwnerTable, row: &Value, max_tokens: usize) -> Vec<Part> {
    let mut raw: Vec<(String, &str)> = Vec::new();
    let mut add = |name: &str, column: &str| {
        if let Some(t) = text_field(row, column) {
            raw.push((name.to_string(), t));
        }
    };
    match table {
        EmbeddingOwnerTable::Observations => {
            add("title", "title");
            add("narrative", "narrative");
        }
        EmbeddingOwnerTable::Summaries => {
            add("request", "request");
            add("learned", "learned");
            add("next_steps", "next_steps");
        }
        EmbeddingOwnerTable::Facts => add("value", "value_text"),
        EmbeddingOwnerTable::Procedures => {
            add("trigger", "trigger_text");
            add("body", "body_md");
        }
        EmbeddingOwnerTable::Entities => add("name", "canonical_name"),
        EmbeddingOwnerTable::Documents => return document_parts(row, max_tokens),
    }
    if table == EmbeddingOwnerTable::Observations
        && let Some(facts) = row.get("facts").and_then(Value::as_array)
    {
        for (i, f) in facts.iter().enumerate() {
            if let Some(t) = f.as_str() {
                raw.push((format!("fact:{i}"), t));
            }
        }
    }
    raw.into_iter()
        .filter(|(_, t)| !t.trim().is_empty())
        .map(|(name, t)| Part { name, text: truncate_tokens(t, max_tokens).to_string() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(parts: &[Part]) -> Vec<&str> {
        parts.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn observation_parts() {
        let row = json!({"title": "T", "narrative": "N", "facts": ["a", " ", "c"]});
        let parts = parts_for(EmbeddingOwnerTable::Observations, &row, 512);
        assert_eq!(names(&parts), ["title", "narrative", "fact:0", "fact:2"]);
    }

    #[test]
    fn null_and_missing_columns_are_skipped() {
        let row = json!({"request": "r", "learned": null, "next_steps": "n"});
        let parts = parts_for(EmbeddingOwnerTable::Summaries, &row, 512);
        assert_eq!(names(&parts), ["request", "next_steps"]);
    }

    #[test]
    fn other_tables() {
        let f = parts_for(EmbeddingOwnerTable::Facts, &json!({"value_text": "v"}), 512);
        assert_eq!(names(&f), ["value"]);
        let p = parts_for(EmbeddingOwnerTable::Procedures, &json!({"trigger_text": "t", "body_md": "b"}), 512);
        assert_eq!(names(&p), ["trigger", "body"]);
        let e = parts_for(EmbeddingOwnerTable::Entities, &json!({"canonical_name": "n"}), 512);
        assert_eq!(names(&e), ["name"]);
    }

    #[test]
    fn document_chunks_are_embedded_in_windows_led_by_their_headings() {
        let row = json!({"heading_path": ["Ops", "Runbook"], "content": "é".repeat(5000), "retired_at": null});
        let parts = parts_for(EmbeddingOwnerTable::Documents, &row, 512);
        assert_eq!(names(&parts), ["body:0", "body:1", "body:2"]);
        assert!(parts.iter().all(|p| p.text.starts_with("Ops > Runbook\n") && p.text.chars().count() <= 2048));
        let body: String = parts.iter().map(|p| p.text.strip_prefix("Ops > Runbook\n").unwrap()).collect();
        assert_eq!(body.chars().count(), 5000, "every character of the chunk is in some window");
        let short = parts_for(EmbeddingOwnerTable::Documents, &json!({"heading_path": [], "content": "text"}), 512);
        assert_eq!(short, [Part { name: "body:0".into(), text: "text".into() }]);
    }

    #[test]
    fn a_retired_document_chunk_has_no_parts() {
        let row = json!({"content": "text", "heading_path": [], "retired_at": "2026-10-25T00:00:00Z"});
        assert!(parts_for(EmbeddingOwnerTable::Documents, &row, 512).is_empty());
    }

    #[test]
    fn truncates_to_token_budget_on_char_boundary() {
        let long = "é".repeat(5000);
        let row = json!({"canonical_name": long});
        let parts = parts_for(EmbeddingOwnerTable::Entities, &row, 512);
        assert_eq!(parts[0].text.chars().count(), 2048);
        assert_eq!(truncate_tokens("short", 512), "short");
    }
}
