//! Embedding parts per owner row (spec 13): which texts of a row get a vector, and how they are
//! named in `embeddings.part`.

use memvault_core::schemas::common::EmbeddingOwnerTable;
use serde_json::Value;

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

/// Parts of a row given as its `to_jsonb` form. Empty or whitespace-only texts are skipped;
/// `fact:i` keeps the index of the fact in the observation's `facts` array.
///
/// observations: `title`, `narrative`, `fact:i`; summaries: `request`, `learned`, `next_steps`;
/// facts: `value` (from `value_text`); procedures: `trigger` (from `trigger_text`), `body` (from
/// `body_md`); entities: `name` (from `canonical_name`).
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
    }
    if table == EmbeddingOwnerTable::Observations {
        if let Some(facts) = row.get("facts").and_then(Value::as_array) {
            for (i, f) in facts.iter().enumerate() {
                if let Some(t) = f.as_str() {
                    raw.push((format!("fact:{i}"), t));
                }
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
    fn truncates_to_token_budget_on_char_boundary() {
        let long = "é".repeat(5000);
        let row = json!({"canonical_name": long});
        let parts = parts_for(EmbeddingOwnerTable::Entities, &row, 512);
        assert_eq!(parts[0].text.chars().count(), 2048);
        assert_eq!(truncate_tokens("short", 512), "short");
    }
}
