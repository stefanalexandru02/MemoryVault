//! What the hooks add to the model's context besides the SessionStart block: past work on a file the agent is about to
//! read. Best effort: any error, timeout or empty answer prints nothing, and nothing here blocks the tool call.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::hooks::Hook;
use crate::{project, store, util};

const FILE_CONTEXT_TIMEOUT: Duration = Duration::from_millis(1000);
/// Rows asked for, so that folding duplicates and dropping minor ones still leaves [`FILE_ROWS_SHOWN`].
const FILE_ROWS_FETCHED: usize = 8;
const FILE_ROWS_SHOWN: usize = 3;
/// The observer rates minor changes 0.1-0.3; rows below this are not worth a Read's worth of context.
const LOW_IMPORTANCE: f64 = 0.4;
const TITLE_CHARS: usize = 80;
/// A header and one title cost about this much: with less left in the session's budget the server is not asked.
const MIN_BLOCK_TOKENS: u32 = 30;
const PROMPT_CONTEXT_TIMEOUT: Duration = Duration::from_millis(800);
/// Characters of the prompt sent to the server.
const PROMPT_CHARS: usize = 500;
/// The most the server may add for a prompt; more is not shown.
const PROMPT_CONTEXT_TOKENS: u32 = 250;

/// `PreToolUse` on `Read`: tells the agent what earlier sessions learned about the file it is about to read, as a
/// header and at most [`FILE_ROWS_SHOWN`] titles. It speaks at most once per file, stays within the session's
/// `MEM_FILE_CONTEXT_BUDGET` (0 turns it off) and says nothing about a file whose notes are all minor.
pub fn file_context(hook: &Hook) {
    let budget = hook.app.cfg.file_context_budget;
    if budget == 0 || hook.str("tool_name") != Some("Read") {
        return;
    }
    let Some(path) = hook.input.get("tool_input").and_then(|i| i.get("file_path")).and_then(Value::as_str) else {
        return;
    };
    let (home, sid) = (hook.app.home(), &hook.target.claude_session_id);
    if path.is_empty() || store::file_context_left(home, sid, budget) < MIN_BLOCK_TOKENS {
        return;
    }
    if !store::first_surface(home, sid, path) {
        return;
    }
    let query = format!(
        "/v1/observations/by-file?path={}&project_key={}&limit={FILE_ROWS_FETCHED}",
        util::encode(path),
        util::encode(&hook.target.project_key)
    );
    let Some(body) = hook.app.api.get(&query, FILE_CONTEXT_TIMEOUT).ok().filter(|r| r.ok()).and_then(|r| r.json())
    else {
        return;
    };
    let rows = file_rows(&body);
    if rows.is_empty() {
        return;
    }
    let block = file_block(&display_path(path, &hook.target.cwd), &rows);
    if store::take_file_context_budget(home, sid, util::estimate_tokens(&block), budget) {
        let out = json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "additionalContext": block } });
        println!("{out}");
    }
}

/// `UserPromptSubmit` in `smart` mode: the first prompt of the session goes to the server (the beginning of it, 800 ms
/// at most), which answers with up to five index rows when the prompt clears its relevance bar and with nothing
/// otherwise. Later prompts, other modes, errors and timeouts add nothing, and the prompt goes on regardless.
pub fn prompt_context(hook: &Hook) {
    let Some(prompt) = hook.str("prompt").filter(|p| !p.trim().is_empty()) else { return };
    if !store::take_smart_pending(hook.app.home(), &hook.target.claude_session_id) {
        return;
    }
    let body = json!({
        "prompt": prompt.chars().take(PROMPT_CHARS).collect::<String>(),
        "project_key": hook.target.project_key,
        "device_id": hook.app.cfg.device,
    });
    let Some(resp) = hook.app.api.post("/v1/context/prompt", &body, PROMPT_CONTEXT_TIMEOUT).ok().filter(|r| r.ok())
    else {
        return;
    };
    let block = resp.body.trim();
    if !block.is_empty() && util::estimate_tokens(block) <= PROMPT_CONTEXT_TOKENS {
        let out = json!({ "hookSpecificOutput": { "hookEventName": "UserPromptSubmit", "additionalContext": block } });
        println!("{out}");
    }
}

/// `(id, title)` of the rows worth showing, newest first: minor ones are dropped and a note worded again is folded
/// into the newest.
fn file_rows(body: &Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for row in body.get("rows").and_then(Value::as_array).into_iter().flatten() {
        let (Some(id), Some(title)) = (row.get("id").and_then(Value::as_str), row.get("title").and_then(Value::as_str))
        else {
            continue;
        };
        let minor = row.get("importance").and_then(Value::as_f64).is_some_and(|i| i < LOW_IMPORTANCE);
        if minor || out.iter().any(|(_, shown)| util::similar_titles(shown, title)) {
            continue;
        }
        out.push((id.to_string(), title.to_string()));
        if out.len() == FILE_ROWS_SHOWN {
            break;
        }
    }
    out
}

fn file_block(path: &str, rows: &[(String, String)]) -> String {
    let ids = util::short_ids(&rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>());
    let lines: Vec<String> = rows
        .iter()
        .zip(ids)
        .map(|((_, title), id)| format!("- {} ({id})", util::truncate(title, TITLE_CHARS)))
        .collect();
    format!("memory: earlier notes on {path} (memory_get id for details)\n{}", lines.join("\n"))
}

/// The path relative to the project (git root, else the working directory); as given when it lies elsewhere.
fn display_path(path: &str, cwd: &str) -> String {
    let relative = |root: &Path| Path::new(path).strip_prefix(root).ok().map(|p| p.to_string_lossy().into_owned());
    project::toplevel(cwd)
        .and_then(|root| relative(&root))
        .or_else(|| relative(Path::new(cwd)))
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rows_drop_minor_ones_and_fold_repeated_titles() {
        let body = json!({"rows": [
            {"id": "a-1", "title": "Retry webhooks with backoff", "importance": 0.8},
            {"id": "a-2", "title": "Renamed a local variable", "importance": 0.2},
            {"id": "a-3", "title": "retry webhooks with backoff.", "importance": 0.7},
            {"id": "a-3b", "title": "Retry webhooks with exponential backoff", "importance": 0.7},
            {"id": "a-4", "title": "No importance from an older server"},
            {"id": "a-5", "title": "Second", "importance": 0.5},
            {"id": "a-6", "title": "Third", "importance": 0.5},
        ]});
        let rows = file_rows(&body);
        let titles: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
        assert_eq!(titles, ["Retry webhooks with backoff", "No importance from an older server", "Second"]);
        assert!(file_rows(&json!({"rows": [{"id": "x", "title": "minor", "importance": 0.1}]})).is_empty());
        assert!(file_rows(&json!({})).is_empty());
    }

    #[test]
    fn a_block_is_a_header_and_titles_with_short_ids() {
        let rows = vec![("0193-aaaaaa-111111".to_string(), "x".repeat(200))];
        let block = file_block("src/a.rs", &rows);
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines[0], "memory: earlier notes on src/a.rs (memory_get id for details)");
        assert_eq!(lines[1], format!("- {}… (111111)", "x".repeat(TITLE_CHARS - 1)));
    }

    #[test]
    fn paths_are_shown_relative_to_the_project() {
        assert_eq!(display_path("/work/proj/src/a.rs", "/work/proj"), "src/a.rs");
        assert_eq!(display_path("/elsewhere/a.rs", "/work/proj"), "/elsewhere/a.rs");
    }
}
