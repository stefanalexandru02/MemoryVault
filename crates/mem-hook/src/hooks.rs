//! The hook subcommands (spec 8.3): one per Claude Code hook event.

use std::io::Read;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::{self, Config};
use crate::deliver::{self, App, CLAUDE_CODE, Delivered, Entry, Fail, Target};
use crate::{inject, project, spool, store, util};

const LIVE_BUDGET: Duration = Duration::from_secs(3);
const DRAIN_BUDGET: Duration = Duration::from_millis(1500);
const CONTEXT_TIMEOUT: Duration = Duration::from_secs(3);
const END_BUDGET: Duration = Duration::from_secs(1);

/// Claude Code's hook input plus everything derived from it.
pub struct Hook {
    pub app: App,
    pub input: Value,
    pub target: Target,
}

impl Hook {
    pub fn str(&self, key: &str) -> Option<&str> {
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
                "agent_type": agent_type(&hook.input),
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
        "file-context" => inject::file_context(&hook),
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

/// The `agent_type` of a call made inside a subagent. Claude Code sets `agent_id` only there; the main thread of a
/// session started with `--agent` has an `agent_type` too, but it is not a subagent, so it is not recorded.
fn agent_type(input: &Value) -> Value {
    let in_subagent = text_of(input, &["agent_id"]).is_some();
    input.get("agent_type").filter(|_| in_subagent).cloned().unwrap_or(Value::Null)
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
        "agent_type": agent_type(input),
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
    let mut query = format!(
        "/v1/context?project_key={}&device_id={}&source={source}",
        util::encode(key),
        util::encode(&hook.app.cfg.device)
    );
    let cfg = &hook.app.cfg;
    if let Some(mode) = &cfg.injection {
        query.push_str(&format!("&mode={mode}"));
    }
    if let Some(rows) = cfg.context_rows {
        query.push_str(&format!("&rows={rows}"));
    }
    if !cfg.context_types.is_empty() {
        query.push_str(&format!("&types={}", cfg.context_types.join(",")));
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

fn session_end(hook: &Hook) {
    let reason = hook.str("reason").unwrap_or("other").to_string();
    let entry = Entry::end(&hook.target, &reason);
    if let Some(d) = settle(hook, &entry, END_BUDGET) {
        spawn_observer(hook, &d.session_id, true);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Mapped, map_event, valid_client};

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
