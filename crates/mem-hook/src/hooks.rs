//! The hook subcommands (spec 8.3): one per Claude Code hook event.

use std::io::Read;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::{self, Config};
use crate::deliver::{self, App, CLAUDE_CODE, Delivered, Entry, Fail, Target};
use crate::{project, spool, store, util};

const LIVE_BUDGET: Duration = Duration::from_secs(3);
const DRAIN_BUDGET: Duration = Duration::from_millis(1500);
const CONTEXT_TIMEOUT: Duration = Duration::from_secs(3);
const END_BUDGET: Duration = Duration::from_secs(1);
const FILE_CONTEXT_TIMEOUT: Duration = Duration::from_millis(1500);
const FILE_CONTEXT_ROWS: usize = 5;
/// Characters of an id shown at least, as in the memory tables.
const SHORT_ID_MIN: usize = 6;

/// Claude Code's hook input plus everything derived from it.
struct Hook {
    app: App,
    input: Value,
    target: Target,
}

impl Hook {
    fn str(&self, key: &str) -> Option<&str> {
        self.input.get(key).and_then(Value::as_str)
    }

    fn field(&self, key: &str) -> Value {
        self.input.get(key).cloned().unwrap_or(Value::Null)
    }
}

/// Reads the hook input from stdin: a JSON object.
fn read_input() -> Option<Value> {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    serde_json::from_str::<Value>(&raw).ok()
}

/// Loads the configuration and builds the hook for one input. None when memory is off, the input is not a
/// session event, or the project is excluded. `session_start` makes a missing configuration say so.
fn load(client: &str, session_start: bool, input: impl FnOnce() -> Option<Value>) -> Option<Hook> {
    let home = config::memory_home();
    let Some(cfg) = Config::load(&home) else {
        if session_start {
            store::hint_missing_config(&home);
        }
        return None;
    };
    let _ = util::mkdir_private(&home); // also narrows a home created with looser permissions
    let input = input()?;
    let sid = input.get("session_id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let project_key = project::resolve(&cwd);
    if store::is_excluded(&home, &project_key) {
        return None;
    }
    let claude_session_id = if client == CLAUDE_CODE { sid.to_string() } else { format!("{client}:{sid}") };
    let target =
        Target { harness: client.to_string(), claude_session_id, project_key, cwd, metadata: Default::default() };
    Some(Hook { app: App::new(cfg), input, target })
}

/// Runs one hook. `event` is the subcommand name. Never fails: every problem ends in a spooled
/// request, a dropped one, or nothing at all.
pub fn run(event: &str) {
    let Some(mut hook) = load(CLAUDE_CODE, event == "session-start", read_input) else { return };
    match event {
        "session-start" => session_start(&mut hook),
        "prompt" => {
            let prompt = hook.field("prompt");
            record(&hook, "user_prompt", json!({ "prompt": prompt }), true);
        }
        "tool" => observe_if_window_closed(&hook, record(&hook, "tool_use", tool_payload(&hook.input), false)),
        "tool-failure" => {
            let payload = json!({
                "tool_name": hook.field("tool_name"),
                "tool_input": hook.field("tool_input"),
                "tool_use_id": hook.field("tool_use_id"),
                "error": hook.field("error"),
                "is_interrupt": hook.field("is_interrupt"),
                "agent_id": hook.field("agent_id"),
            });
            observe_if_window_closed(&hook, record(&hook, "tool_failure", payload, false));
        }
        "stop" => {
            // A stop hook already forced the agent to continue: this stop is not a real turn end.
            if hook.input.get("stop_hook_active").and_then(Value::as_bool).unwrap_or(false) {
                return;
            }
            let payload = json!({
                "last_assistant_message": hook.field("last_assistant_message"),
                "stop_hook_active": false,
            });
            if let Some(d) = record(&hook, "assistant_message", payload, false) {
                spawn_observer(&hook, &d.session_id, false);
            }
        }
        "subagent-stop" => {
            let payload = json!({
                "agent_id": hook.field("agent_id"),
                "agent_type": hook.field("agent_type"),
                "last_assistant_message": hook.field("last_assistant_message"),
            });
            record(&hook, "subagent_stop", payload, false);
        }
        "session-end" => session_end(&hook),
        "file-context" => file_context(&hook),
        _ => eprintln!("mem-hook: unknown event {event:?}"),
    }
}

/// What a generic-client event asks for.
#[derive(Debug, PartialEq)]
enum Mapped {
    Start,
    End,
    Prompt(Value),
    Tool(Value),
    Assistant(Value),
}

/// The first non-empty string among `keys` of the input.
fn text_of<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| input.get(*k).and_then(Value::as_str).filter(|t| !t.is_empty()))
}

/// Maps the input of an `event --client` call onto the events the Claude Code hooks produce. The event is the
/// `event` field (`start`, `prompt`, `tool`, `assistant`, `end`), else the client's own `hook_event_name`
/// (Codex CLI and Gemini CLI hook inputs work as they are). None when there is nothing to record.
fn map_event(input: &Value) -> Option<Mapped> {
    let name = text_of(input, &["event", "hook_event_name"])?;
    match name {
        "start" | "SessionStart" => Some(Mapped::Start),
        "end" | "SessionEnd" => Some(Mapped::End),
        "prompt" | "UserPromptSubmit" | "BeforeAgent" => {
            let prompt = text_of(input, &["prompt", "text"])?;
            Some(Mapped::Prompt(json!({ "prompt": prompt })))
        }
        "tool" | "PostToolUse" | "AfterTool" => {
            text_of(input, &["tool_name"])?;
            Some(Mapped::Tool(tool_payload(input)))
        }
        "assistant" | "Stop" | "AfterAgent" => {
            // A hook that already made the agent continue: this is not a real turn end.
            if input.get("stop_hook_active").and_then(Value::as_bool).unwrap_or(false) {
                return None;
            }
            let text = text_of(input, &["last_assistant_message", "prompt_response", "text"])?;
            Some(Mapped::Assistant(json!({ "last_assistant_message": text, "stop_hook_active": false })))
        }
        _ => None,
    }
}

/// A client name is lowercase letters, digits, `-` and `_`, at most 32 characters.
fn valid_client(name: &str) -> bool {
    name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_".contains(c))
}

/// `mem-hook event --client <name>`: one event of another agent CLI, on stdin, through the same spool and
/// delivery as the Claude Code hooks. Never fails the host.
pub fn run_generic(client: &str) {
    if !valid_client(client) {
        return eprintln!("mem-hook: invalid --client {client:?} (lowercase letters, digits, - and _; at most 32)");
    }
    let Some(input) = read_input() else { return };
    let Some(mapped) = map_event(&input) else {
        return util::debug("event ignored: unknown or nothing to record");
    };
    let Some(mut hook) = load(client, mapped == Mapped::Start, || Some(input)) else { return };
    match mapped {
        Mapped::Start => session_start(&mut hook),
        Mapped::End => session_end(&hook),
        Mapped::Prompt(payload) => {
            record(&hook, "user_prompt", payload, true);
        }
        Mapped::Tool(payload) => observe_if_window_closed(&hook, record(&hook, "tool_use", payload, false)),
        Mapped::Assistant(payload) => {
            if let Some(d) = record(&hook, "assistant_message", payload, false) {
                spawn_observer(&hook, &d.session_id, false);
            }
        }
    }
}

fn tool_payload(input: &Value) -> Value {
    let field = |key: &str| input.get(key).cloned().unwrap_or(Value::Null);
    json!({
        "tool_name": field("tool_name"),
        "tool_input": field("tool_input"),
        "tool_response": field("tool_response"),
        "tool_use_id": field("tool_use_id"),
        "duration_ms": field("duration_ms"),
        "agent_id": field("agent_id"),
    })
}

/// Posts one event; on a retryable failure the request goes to the spool.
fn record(hook: &Hook, kind: &str, payload: Value, new_prompt: bool) -> Option<Delivered> {
    let home = hook.app.home();
    let started = Instant::now();
    deliver::prepare(&hook.app, &hook.target, LIVE_BUDGET);
    let seq = store::alloc_seq(home, &hook.target.claude_session_id, new_prompt);
    let entry = Entry::event(&hook.target, seq, kind, payload);
    settle(hook, &entry, LIVE_BUDGET.saturating_sub(started.elapsed()))
}

fn settle(hook: &Hook, entry: &Entry, budget: Duration) -> Option<Delivered> {
    match deliver::send(&hook.app, entry, budget) {
        Ok(d) => Some(d),
        Err(Fail::Retry(why) | Fail::Server(why)) => {
            util::debug(&format!("spooling {:?}: {why}", entry.kind));
            spool::append(hook.app.home(), entry);
            None
        }
        Err(Fail::Drop(msg)) => {
            eprintln!("memory: server rejected the event: {msg}");
            None
        }
    }
}

fn observe_if_window_closed(hook: &Hook, delivered: Option<Delivered>) {
    if let Some(d) = delivered.filter(|d| d.window_closed) {
        spawn_observer(hook, &d.session_id, false);
    }
}

/// Starts `mem-hook observe --session <id> [--final]` detached from this process. The observer runs the
/// `claude` CLI, so a machine of another agent CLI that has none (no Claude Code install) leaves the work
/// packets for a machine that has.
fn spawn_observer(hook: &Hook, session_id: &str, final_run: bool) {
    if hook.target.harness != CLAUDE_CODE && !util::on_path(&hook.app.cfg.claude_bin) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut args = vec!["observe", "--session", session_id];
    if final_run {
        args.push("--final");
    }
    util::spawn_detached(&exe.to_string_lossy(), &args);
}

fn session_start(hook: &mut Hook) {
    let home = hook.app.home().to_path_buf();
    store::prune_sessions(&home);
    spool::drain(&hook.app, DRAIN_BUDGET);

    let source = match hook.str("source") {
        Some(s @ ("startup" | "resume" | "clear" | "compact")) => s.to_string(),
        _ => "startup".to_string(),
    };
    hook.target.metadata.insert("source".into(), json!(source));
    hook.target.metadata.insert("model".into(), hook.field("model"));
    let payload = json!({ "source": source, "model": hook.field("model") });
    record(hook, "system", payload, false);

    // Like claude-mem, no context on resume: the conversation already has it.
    if source == "resume" {
        return;
    }
    let key = &hook.target.project_key;
    let mut query =
        format!("/v1/context?project_key={}&device_id={}&source={source}", encode(key), encode(&hook.app.cfg.device));
    if let Some(mode) = &hook.app.cfg.injection {
        query.push_str("&mode=");
        query.push_str(mode);
    }
    let fetched = hook.app.api.get(&query, CONTEXT_TIMEOUT).ok().filter(|r| r.ok());
    let block = match fetched {
        Some(resp) => {
            store::save_context(&home, key, &resp.body);
            resp.body
        }
        None => {
            let path = store::context_cache_path(&home, key);
            match std::fs::read_to_string(&path) {
                Ok(cached) if !cached.trim().is_empty() => {
                    let when = util::mtime_rfc3339(&path).unwrap_or_else(|| "unknown time".into());
                    format!("> memory: server unreachable, showing cached context from {when}\n\n{cached}")
                }
                _ => return,
            }
        }
    };
    if block.trim().is_empty() {
        return;
    }
    let out = json!({ "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": block } });
    println!("{out}");
}

/// `PreToolUse` on `Read`: tells the agent what earlier sessions learned about the file it is about to read.
/// It never blocks or rewrites the read, and speaks at most once per file per session.
fn file_context(hook: &Hook) {
    if hook.str("tool_name") != Some("Read") {
        return;
    }
    let Some(path) = hook.input.get("tool_input").and_then(|i| i.get("file_path")).and_then(Value::as_str) else {
        return;
    };
    if path.is_empty() || !store::first_surface(hook.app.home(), &hook.target.claude_session_id, path) {
        return;
    }
    let query = format!(
        "/v1/observations/by-file?path={}&project_key={}&limit={FILE_CONTEXT_ROWS}",
        encode(path),
        encode(&hook.target.project_key)
    );
    let Some(rows) = hook
        .app
        .api
        .get(&query, FILE_CONTEXT_TIMEOUT)
        .ok()
        .filter(|r| r.ok())
        .and_then(|r| r.json())
        .and_then(|v| v.get("rows").and_then(Value::as_array).cloned())
    else {
        return;
    };
    let rows: Vec<(&str, &str, &str, &str)> = rows
        .iter()
        .filter_map(|row| {
            let id = row.get("id")?.as_str()?;
            let title = row.get("title")?.as_str()?;
            let day = row.get("created_at").and_then(Value::as_str).map(|t| &t[..t.len().min(10)]).unwrap_or("");
            let kind = row.get("type").and_then(Value::as_str).unwrap_or("note");
            Some((id, day, kind, title))
        })
        .collect();
    let ids = short_ids(&rows.iter().map(|r| r.0).collect::<Vec<_>>());
    let lines: Vec<String> =
        rows.iter().zip(ids).map(|((_, day, kind, title), id)| format!("- {day} [{kind}] {title} (id {id})")).collect();
    if lines.is_empty() {
        return;
    }
    let block = format!(
        "memory: earlier sessions touched {path}, newest first. Fetch details with memory_get when one matters:\n{}",
        lines.join("\n")
    );
    let out = json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "additionalContext": block } });
    println!("{out}");
}

fn session_end(hook: &Hook) {
    let reason = hook.str("reason").unwrap_or("other").to_string();
    let entry = Entry::end(&hook.target, &reason);
    if let Some(d) = settle(hook, &entry, END_BUDGET) {
        spawn_observer(hook, &d.session_id, true);
    }
}

/// The last characters of each id (hyphens ignored) that tell it from the others, at least [`SHORT_ID_MIN`]; the
/// memory tools take such an ending in place of the whole id.
fn short_ids(ids: &[&str]) -> Vec<String> {
    let plain: Vec<String> = ids.iter().map(|id| id.replace('-', "")).collect();
    let tail = |id: &str, len: usize| id.get(id.len().saturating_sub(len)..).unwrap_or(id).to_string();
    plain
        .iter()
        .map(|own| {
            let len = (SHORT_ID_MIN..own.len())
                .find(|&len| plain.iter().all(|other| other == own || tail(other, len) != tail(own, len)))
                .unwrap_or(own.len());
            tail(own, len)
        })
        .collect()
}

/// Percent-encodes everything but unreserved characters.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Mapped, map_event, short_ids, valid_client};

    #[test]
    fn short_ids_keep_six_characters_unless_rows_end_alike() {
        let ids = [
            "01930000-0000-7000-8000-0000001a1b2c3",
            "01930000-0000-7000-8000-0000002a1b2c3",
            "01930000-0000-7000-8000-000000ffffff",
        ];
        assert_eq!(short_ids(&ids), ["1a1b2c3", "2a1b2c3", "ffffff"]);
        assert_eq!(short_ids(&["x"]), ["x"]);
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(super::encode("github.com/alex/home lab"), "github.com%2Falex%2Fhome%20lab");
    }

    #[test]
    fn generic_events_map_onto_the_hook_events() {
        let m = |v| map_event(&v);
        assert_eq!(m(json!({"event": "start", "session_id": "s"})), Some(Mapped::Start));
        assert_eq!(m(json!({"event": "end", "reason": "exit"})), Some(Mapped::End));
        assert_eq!(m(json!({"event": "prompt", "text": "fix it"})), Some(Mapped::Prompt(json!({"prompt": "fix it"}))));
        assert_eq!(
            m(json!({"event": "assistant", "text": "done"})),
            Some(Mapped::Assistant(json!({"last_assistant_message": "done", "stop_hook_active": false})))
        );
        let Some(Mapped::Tool(p)) = m(json!({"event": "tool", "tool_name": "ls", "tool_input": {"a": 1}})) else {
            panic!("not a tool event");
        };
        assert_eq!((p["tool_name"].as_str(), p["tool_input"]["a"].as_i64()), (Some("ls"), Some(1)));
        assert!(p["tool_response"].is_null());
    }

    #[test]
    fn codex_and_gemini_hook_inputs_work_unchanged() {
        let m = |v| map_event(&v);
        assert_eq!(m(json!({"hook_event_name": "SessionStart", "source": "startup"})), Some(Mapped::Start));
        assert_eq!(m(json!({"hook_event_name": "SessionEnd", "reason": "exit"})), Some(Mapped::End));
        // Gemini: the prompt is on BeforeAgent, the answer in prompt_response on AfterAgent (which also repeats the prompt).
        assert_eq!(
            m(json!({"hook_event_name": "BeforeAgent", "prompt": "hi"})),
            Some(Mapped::Prompt(json!({"prompt": "hi"})))
        );
        assert_eq!(
            m(json!({"hook_event_name": "AfterAgent", "prompt": "hi", "prompt_response": "hello"})),
            Some(Mapped::Assistant(json!({"last_assistant_message": "hello", "stop_hook_active": false})))
        );
        assert!(matches!(m(json!({"hook_event_name": "AfterTool", "tool_name": "read_file"})), Some(Mapped::Tool(_))));
        // Codex: UserPromptSubmit, PostToolUse and Stop with last_assistant_message.
        assert!(matches!(m(json!({"hook_event_name": "UserPromptSubmit", "prompt": "hi"})), Some(Mapped::Prompt(_))));
        assert!(matches!(m(json!({"hook_event_name": "PostToolUse", "tool_name": "shell"})), Some(Mapped::Tool(_))));
        assert_eq!(
            m(json!({"hook_event_name": "Stop", "last_assistant_message": "ok", "stop_hook_active": false})),
            Some(Mapped::Assistant(json!({"last_assistant_message": "ok", "stop_hook_active": false})))
        );
        // The explicit `event` wins over the client's own name.
        assert_eq!(m(json!({"event": "end", "hook_event_name": "Stop"})), Some(Mapped::End));
    }

    #[test]
    fn generic_events_without_content_are_ignored() {
        let m = |v| map_event(&v);
        assert_eq!(m(json!({})), None);
        assert_eq!(m(json!({"event": "nonsense"})), None);
        assert_eq!(m(json!({"event": "prompt", "text": ""})), None);
        assert_eq!(m(json!({"event": "prompt"})), None);
        assert_eq!(m(json!({"event": "tool", "tool_input": {}})), None);
        assert_eq!(m(json!({"event": "assistant", "last_assistant_message": null})), None);
        assert_eq!(
            m(json!({"hook_event_name": "Stop", "last_assistant_message": "x", "stop_hook_active": true})),
            None
        );
        assert_eq!(m(json!({"event": 5})), None);
    }

    #[test]
    fn client_names_are_restricted() {
        for ok in ["codex", "gemini", "my-agent_2", "claude-code"] {
            assert!(valid_client(ok), "{ok}");
        }
        for bad in ["", "Codex", "-x", "a b", "a/b", "../x", &"a".repeat(33)] {
            assert!(!valid_client(bad), "{bad}");
        }
    }
}
