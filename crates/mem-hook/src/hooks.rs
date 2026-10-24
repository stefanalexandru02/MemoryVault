//! The hook subcommands (spec 8.3): one per Claude Code hook event.

use std::io::Read;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{self, Config};
use crate::deliver::{self, App, Delivered, Entry, Fail, Target};
use crate::{project, spool, store, util};

const LIVE_BUDGET: Duration = Duration::from_secs(3);
const DRAIN_BUDGET: Duration = Duration::from_millis(1500);
const CONTEXT_TIMEOUT: Duration = Duration::from_secs(3);
const END_BUDGET: Duration = Duration::from_secs(1);

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

/// Runs one hook. `event` is the subcommand name. Never fails: every problem ends in a spooled
/// request, a dropped one, or nothing at all.
pub fn run(event: &str) {
    let home = config::memory_home();
    let Some(cfg) = Config::load(&home) else {
        if event == "session-start" {
            store::hint_missing_config(&home);
        }
        return;
    };
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let Ok(input) = serde_json::from_str::<Value>(&raw) else { return };
    let Some(sid) = input.get("session_id").and_then(Value::as_str).filter(|s| !s.is_empty()) else { return };
    let cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let project_key = project::resolve(&cwd);
    if store::is_excluded(&home, &project_key) {
        return;
    }
    let target = Target { claude_session_id: sid.to_string(), project_key, cwd, metadata: Default::default() };
    let mut hook = Hook { app: App::new(cfg), input, target };
    match event {
        "session-start" => session_start(&mut hook),
        "prompt" => {
            let prompt = hook.field("prompt");
            record(&hook, "user_prompt", json!({ "prompt": prompt }), true);
        }
        "tool" => {
            let payload = json!({
                "tool_name": hook.field("tool_name"),
                "tool_input": hook.field("tool_input"),
                "tool_response": hook.field("tool_response"),
                "tool_use_id": hook.field("tool_use_id"),
                "duration_ms": hook.field("duration_ms"),
                "agent_id": hook.field("agent_id"),
            });
            observe_if_window_closed(record(&hook, "tool_use", payload, false));
        }
        "tool-failure" => {
            let payload = json!({
                "tool_name": hook.field("tool_name"),
                "tool_input": hook.field("tool_input"),
                "tool_use_id": hook.field("tool_use_id"),
                "error": hook.field("error"),
                "is_interrupt": hook.field("is_interrupt"),
                "agent_id": hook.field("agent_id"),
            });
            observe_if_window_closed(record(&hook, "tool_failure", payload, false));
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
                spawn_observer(&d.session_id, false);
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
        _ => eprintln!("mem-hook: unknown event {event:?}"),
    }
}

/// Posts one event; on a retryable failure the request goes to the spool.
fn record(hook: &Hook, kind: &str, payload: Value, new_prompt: bool) -> Option<Delivered> {
    let home = hook.app.home();
    let seq = store::alloc_seq(home, &hook.target.claude_session_id, new_prompt);
    let entry = Entry::event(&hook.target, seq, kind, payload);
    settle(hook, &entry, LIVE_BUDGET)
}

fn settle(hook: &Hook, entry: &Entry, budget: Duration) -> Option<Delivered> {
    match deliver::send(&hook.app, entry, budget) {
        Ok(d) => Some(d),
        Err(Fail::Retry(why)) => {
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

fn observe_if_window_closed(delivered: Option<Delivered>) {
    if let Some(d) = delivered.filter(|d| d.window_closed) {
        spawn_observer(&d.session_id, false);
    }
}

/// Starts `mem-hook observe --session <id> [--final]` detached from this process.
fn spawn_observer(session_id: &str, final_run: bool) {
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
    let query =
        format!("/v1/context?project_key={}&device_id={}&source={source}", encode(key), encode(&hook.app.cfg.device));
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
        spawn_observer(&d.session_id, true);
    }
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
    #[test]
    fn encodes_query_values() {
        assert_eq!(super::encode("github.com/alex/home lab"), "github.com%2Falex%2Fhome%20lab");
    }
}
