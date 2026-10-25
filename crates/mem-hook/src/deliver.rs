//! Delivering one logical request (an event or a session end) to the server, creating the server
//! session first when needed. Used live by the hooks and by the spool replay.

use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::{Api, Resp};
use crate::config::Config;
use crate::store;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

pub struct App {
    pub cfg: Config,
    pub api: Api,
}

impl App {
    pub fn new(cfg: Config) -> App {
        let api = Api::new(&cfg);
        App { cfg, api }
    }

    pub fn home(&self) -> &Path {
        &self.cfg.home
    }
}

/// Identifies the session an entry belongs to; also what is needed to create it on the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub claude_session_id: String,
    pub project_key: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Event,
    End,
}

/// One spool line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    #[serde(flatten)]
    pub target: Target,
    pub op: Op,
    pub occurred_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Replays the server answered 500 to while it accepted other requests (spool bookkeeping).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub replays: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Entry {
    pub fn event(target: &Target, seq: i64, kind: &str, mut payload: Value) -> Entry {
        sanitize(&mut payload);
        Entry {
            target: target.clone(),
            op: Op::Event,
            occurred_at: crate::util::now_rfc3339(),
            seq: Some(seq),
            kind: Some(kind.into()),
            payload,
            reason: None,
            replays: 0,
        }
    }

    pub fn end(target: &Target, reason: &str) -> Entry {
        Entry {
            target: target.clone(),
            op: Op::End,
            occurred_at: crate::util::now_rfc3339(),
            seq: None,
            kind: None,
            payload: Value::Null,
            reason: Some(reason.into()),
            replays: 0,
        }
    }
}

const CLAMP_OVER: usize = 65_536;
const CLAMP_HEAD: usize = 32_768;
const CLAMP_TAIL: usize = 8_192;

/// Makes a payload deliverable: NUL characters (binary tool output) are dropped from every string
/// and key, because the server's `jsonb` cannot store them; and a string over 64k chars keeps its
/// head and tail around the same marker the server uses (the server clamps further, per field), so
/// request bodies stay far below the 4 MB batch limit.
fn sanitize(v: &mut Value) {
    match v {
        Value::String(s) => {
            if s.contains('\0') {
                *s = s.replace('\0', "");
            }
            if s.chars().count() > CLAMP_OVER {
                let total = s.chars().count();
                let head: String = s.chars().take(CLAMP_HEAD).collect();
                let tail: String = s.chars().skip(total - CLAMP_TAIL).collect();
                *s = format!("{head}…[clamped {} chars]…{tail}", total - CLAMP_HEAD - CLAMP_TAIL);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(sanitize),
        Value::Object(o) => {
            if o.keys().any(|k| k.contains('\0')) {
                *o = std::mem::take(o).into_iter().map(|(k, v)| (k.replace('\0', ""), v)).collect();
            }
            o.values_mut().for_each(sanitize);
        }
        _ => {}
    }
}

/// Why a delivery did not complete.
#[derive(Debug)]
pub enum Fail {
    /// Try again later: spool it.
    Retry(String),
    /// The server answered 500: spool it like [`Fail::Retry`], but a replay may skip it (a request
    /// the server chokes on must not block the ones behind it).
    Server(String),
    /// The server will never accept it: drop it.
    Drop(String),
}

pub struct Delivered {
    pub session_id: String,
    pub window_closed: bool,
}

fn judge(resp: &Resp) -> Fail {
    let msg = format!("HTTP {}: {}", resp.status, resp.body.chars().take(200).collect::<String>());
    match resp.status {
        500 => Fail::Server(msg),
        401 | 403 | 404 | 408 | 425 | 429 | 501.. => Fail::Retry(msg),
        _ => Fail::Drop(msg),
    }
}

struct Deadline(Instant);

impl Deadline {
    /// Timeout for the next request, or Retry when the budget is spent.
    fn next(&self) -> Result<Duration, Fail> {
        let left = self.0.saturating_duration_since(Instant::now());
        if left < Duration::from_millis(50) {
            return Err(Fail::Retry("time budget exhausted".into()));
        }
        Ok(left.min(REQUEST_TIMEOUT))
    }
}

/// The server session id for `target`, created (idempotently) when the cache has none.
fn ensure_session(app: &App, target: &Target, deadline: &Deadline) -> Result<String, Fail> {
    let home = app.home();
    if let Some(id) = store::update_cache(home, &target.claude_session_id, |c| c.session_id.clone()) {
        return Ok(id);
    }
    let body = json!({
        "harness": "claude-code",
        "external_session_id": target.claude_session_id,
        "project_key": target.project_key,
        "cwd": target.cwd,
        "metadata": target.metadata,
    });
    let resp = app.api.post("/v1/sessions", &body, deadline.next()?).map_err(Fail::Retry)?;
    if !resp.ok() {
        return Err(judge(&resp));
    }
    let reply = resp.json().unwrap_or(Value::Null);
    let id = reply
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Fail::Drop("session response without session_id".into()))?;
    // A session that already holds events (resumed after the local counter was pruned) continues
    // from the server's count instead of colliding with its stored events.
    let next_seq = reply.get("next_seq").and_then(Value::as_i64).unwrap_or(0);
    store::update_cache(home, &target.claude_session_id, |c| {
        c.session_id = Some(id.clone());
        c.next_seq = c.next_seq.max(next_seq);
    });
    Ok(id)
}

/// Makes sure the local cache knows the server session (creating it) before the first sequence
/// number of a session is taken, so a lost counter is rebuilt from the server. Best effort: offline,
/// the counter just continues from what the cache holds.
pub fn prepare(app: &App, target: &Target, budget: Duration) {
    let _ = ensure_session(app, target, &Deadline(Instant::now() + budget));
}

/// Posts the entry. `budget` bounds the whole call (session creation included).
pub fn send(app: &App, entry: &Entry, budget: Duration) -> Result<Delivered, Fail> {
    let deadline = Deadline(Instant::now() + budget);
    match send_once(app, entry, &deadline) {
        // The server forgot the session (cache cleared): create it again, once.
        Err(Fail::Retry(m)) if m.starts_with("HTTP 404") => send_once(app, entry, &deadline).map_err(|e| match e {
            Fail::Retry(m2) if m2.starts_with("HTTP 404") => Fail::Drop(m2),
            e => e,
        }),
        r => r,
    }
}

fn send_once(app: &App, entry: &Entry, deadline: &Deadline) -> Result<Delivered, Fail> {
    let session_id = ensure_session(app, &entry.target, deadline)?;
    let (path, body) = match entry.op {
        Op::Event => {
            let seq = entry.seq.unwrap_or(0);
            // Lines spooled by an older version may still carry NUL characters.
            let mut payload = entry.payload.clone();
            sanitize(&mut payload);
            let event = json!({
                "seq": seq,
                "kind": entry.kind,
                "occurred_at": entry.occurred_at,
                "idempotency_key": format!("{}:{seq}", entry.target.claude_session_id),
                "payload": payload,
            });
            (format!("/v1/sessions/{session_id}/events"), json!({ "events": [event] }))
        }
        Op::End => (
            format!("/v1/sessions/{session_id}/end"),
            json!({ "reason": entry.reason, "occurred_at": entry.occurred_at }),
        ),
    };
    let resp = app.api.post(&path, &body, deadline.next()?).map_err(Fail::Retry)?;
    if !resp.ok() {
        if resp.status == 404 {
            // The server forgot the session: create it again on the next attempt.
            store::update_cache(app.home(), &entry.target.claude_session_id, |c| c.session_id = None);
        }
        return Err(judge(&resp));
    }
    store::touch_delivery(app.home());
    let reply = resp.json().unwrap_or(Value::Null);
    if let Some(w) = reply.get("window_no").and_then(Value::as_i64) {
        store::update_cache(app.home(), &entry.target.claude_session_id, |c| c.window = w);
    }
    Ok(Delivered { session_id, window_closed: reply.get("window_closed").and_then(Value::as_bool).unwrap_or(false) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_strings_are_clamped() {
        let mut v = json!({"a": ["x".repeat(100_000)], "b": "short"});
        sanitize(&mut v);
        let a = v["a"][0].as_str().unwrap();
        assert!(a.contains("…[clamped 59040 chars]…"));
        assert_eq!(a.chars().count(), CLAMP_HEAD + CLAMP_TAIL + "…[clamped 59040 chars]…".chars().count());
        assert_eq!(v["b"], "short");
    }

    #[test]
    fn nul_characters_are_dropped_from_strings_and_keys() {
        let mut v = json!({"out": "a\u{0}b", "k\u{0}": [{"x": "\u{0}"}]});
        sanitize(&mut v);
        assert_eq!(v, json!({"out": "ab", "k": [{"x": ""}]}));
    }
}
