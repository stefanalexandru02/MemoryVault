//! A recording HTTP server standing in for the memory API.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const SESSION_ID: &str = "11111111-1111-1111-1111-111111111111";

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub struct State {
    pub reqs: Vec<Req>,
    /// Every request is answered 503.
    pub down: bool,
    pub window_closed: bool,
    /// Status for event posts (202 = accept).
    pub events_status: u16,
    /// Accept this many more event posts, then answer 503 (None = unlimited).
    pub events_ok: Option<usize>,
    /// `next_seq` reported by `POST /v1/sessions`.
    pub next_seq: i64,
    /// Event posts whose body contains this text are answered 500 (a request the server chokes on).
    pub poison: Option<String>,
    pub context: Option<String>,
    pub packets: VecDeque<Value>,
    /// Replies for `POST /v1/work/{id}/result`; empty = 200.
    pub result_replies: VecDeque<(u16, Value)>,
    /// Status of `GET /v1/me` (200 = a device token, 401 = rejected).
    pub me_status: u16,
    /// Reply of `GET /v1/admin/doctor`.
    pub admin_doctor: (u16, Value),
    /// What `GET /v1/sources` lists.
    pub sources: Vec<Value>,
    /// The next this many `POST /v1/sources` are answered 429 (with `Retry-After: 0`).
    pub sources_throttled: usize,
    /// A `POST /v1/sources` whose uri contains this text is answered 413.
    pub sources_reject: Option<String>,
}

#[derive(Clone)]
pub struct Fake {
    pub url: String,
    state: Arc<Mutex<State>>,
}

impl Fake {
    pub fn start() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            reqs: vec![],
            down: false,
            window_closed: false,
            events_status: 202,
            events_ok: None,
            next_seq: 0,
            poison: None,
            context: None,
            packets: VecDeque::new(),
            result_replies: VecDeque::new(),
            me_status: 200,
            admin_doctor: (
                403,
                json!({"error": {"code": "insufficient_scope", "message": "admin", "retryable": false}}),
            ),
            sources: vec![],
            sources_throttled: 0,
            sources_reject: None,
        }));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = shared.clone();
                std::thread::spawn(move || handle(stream, &shared));
            }
        });
        Fake { url, state }
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn reqs(&self) -> Vec<Req> {
        self.state().reqs.clone()
    }

    /// Requests whose "METHOD path-prefix" matches.
    pub fn matching(&self, method: &str, prefix: &str) -> Vec<Req> {
        self.reqs().into_iter().filter(|r| r.method == method && r.path.starts_with(prefix)).collect()
    }

    /// All posted events in arrival order.
    pub fn events(&self) -> Vec<Value> {
        self.reqs()
            .iter()
            .filter(|r| r.method == "POST" && r.path.ends_with("/events"))
            .flat_map(|r| r.body["events"].as_array().cloned().unwrap_or_default())
            .collect()
    }

    pub fn wait_for(&self, what: &str, cond: impl Fn(&[Req]) -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if cond(&self.state().reqs) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("timed out waiting for {what}; requests: {:#?}", self.reqs());
    }

    /// Bodies of the posted sources in arrival order.
    pub fn posted_sources(&self) -> Vec<Value> {
        self.matching("POST", "/v1/sources").into_iter().map(|r| r.body).collect()
    }

    pub fn queue_packet(&self, packet: Value) {
        self.state().packets.push_back(packet);
    }
}

pub fn packet(id: &str, kind: &str, hint: &str) -> Value {
    json!({
        "id": id, "kind": kind, "prompt_ref": format!("{kind}@v1"), "model_hint": hint,
        "inputs": {}, "output_schema": "observer.v1", "lease_until": "2030-01-01T00:00:00Z", "attempts": 0,
        "prompt": { "system": "SYSTEM PROMPT", "user_template": "tpl", "user": "USER PROMPT\nline two" },
    })
}

fn handle(mut stream: TcpStream, shared: &Arc<Mutex<State>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (first.next().unwrap_or_default().to_string(), first.next().unwrap_or_default().to_string());
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + length {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body: Value = serde_json::from_slice(&buf[header_end..]).unwrap_or(Value::Null);
    let req = Req { method, path, headers, body };

    let (status, ctype, reply) = route(&mut shared.lock().unwrap(), req);
    let text = reply.to_string();
    let text = if ctype == "text/markdown" { reply.as_str().unwrap_or_default().to_string() } else { text };
    let retry_after = if status == 429 { "Retry-After: 0\r\n" } else { "" };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\n{retry_after}Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

fn route(s: &mut State, req: Req) -> (u16, &'static str, Value) {
    let (method, path, body) = (req.method.clone(), req.path.clone(), req.body.clone());
    s.reqs.push(req);
    let json = "application/json";
    if s.down {
        return (503, json, json!({"error": {"code": "db_unavailable", "message": "down", "retryable": true}}));
    }
    let route = path.split('?').next().unwrap_or_default().to_string();
    let segs: Vec<&str> = route.trim_matches('/').split('/').collect();
    match (method.as_str(), segs.as_slice()) {
        ("GET", ["health"]) => (200, json, json!({"status": "ok", "db": true})),
        ("GET", ["v1", "me"]) if s.me_status == 200 => (
            200,
            json,
            json!({"device_id": "laptop-x1", "token_kind": "device", "scopes": ["events:write", "memories:read", "work:lease"]}),
        ),
        ("GET", ["v1", "me"]) => {
            (s.me_status, json, json!({"error": {"code": "invalid_token", "message": "no", "retryable": false}}))
        }
        ("GET", ["v1", "admin", "doctor"]) => (s.admin_doctor.0, json, s.admin_doctor.1.clone()),
        ("POST", ["v1", "sessions"]) => (
            201,
            json,
            json!({"session_id": SESSION_ID, "space_id": SESSION_ID, "window_policy": {"tool_uses": 8, "idle_seconds": 90},
                   "next_seq": s.next_seq}),
        ),
        ("POST", ["v1", "sessions", _, "events"]) => {
            let n = body["events"].as_array().map_or(0, Vec::len);
            let status = s.events_status;
            if s.poison.as_ref().is_some_and(|m| body.to_string().contains(m.as_str())) {
                return (500, json, json!({"error": {"code": "internal", "message": "boom", "retryable": true}}));
            }
            if let Some(left) = s.events_ok.as_mut() {
                if *left == 0 {
                    return (
                        503,
                        json,
                        json!({"error": {"code": "db_unavailable", "message": "flaky", "retryable": true}}),
                    );
                }
                *left -= 1;
            }
            if status != 202 {
                return (
                    status,
                    json,
                    json!({"error": {"code": "invalid_request", "message": "no", "retryable": false}}),
                );
            }
            (202, json, json!({"accepted": n, "duplicates": 0, "window_no": 1, "window_closed": s.window_closed}))
        }
        ("POST", ["v1", "sessions", _, "end"]) => (
            200,
            json,
            json!({"session_id": SESSION_ID, "ended_at": "2030-01-01T00:00:00Z", "window_closed": true, "packets_created": []}),
        ),
        ("GET", ["v1", "context"]) => match &s.context {
            Some(c) => (200, "text/markdown", Value::String(c.clone())),
            None => (404, json, json!({"error": {"code": "not_found", "message": "none", "retryable": false}})),
        },
        ("GET", ["v1", "sources"]) => (200, json, json!({"sources": s.sources})),
        ("POST", ["v1", "sources"]) => {
            if s.sources_throttled > 0 {
                s.sources_throttled -= 1;
                return (
                    429,
                    json,
                    json!({"error": {"code": "rate_limited", "message": "slow down", "retryable": true}}),
                );
            }
            let uri = body["uri"].as_str().unwrap_or_default();
            if s.sources_reject.as_ref().is_some_and(|m| uri.contains(m.as_str())) {
                let error = json!({"code": "payload_too_large", "message": "content is too big", "retryable": false});
                return (413, json, json!({ "error": error }));
            }
            let listed = s.sources.iter().any(|l| l["uri"] == body["uri"] && l["content_hash"] == body["content_hash"]);
            if listed {
                return (200, json, json!({"uri": uri, "status": "unchanged", "chunks": 1, "retired": 0}));
            }
            (201, json, json!({"uri": uri, "status": "created", "chunks": 1, "retired": 0}))
        }
        ("DELETE", ["v1", "sources"]) => (200, json, json!({"deleted": 1})),
        ("POST", ["v1", "work", "lease"]) => {
            (200, json, json!({"packets": s.packets.pop_front().into_iter().collect::<Vec<_>>()}))
        }
        ("POST", ["v1", "work", _, "result"]) => {
            let (status, reply) =
                s.result_replies.pop_front().unwrap_or((200, json!({"applied": {"observations": 1}})));
            (status, json, reply)
        }
        ("POST", ["v1", "work", _, "fail"]) => (200, json, json!({"status": "pending"})),
        _ => (404, json, json!({"error": {"code": "not_found", "message": "route", "retryable": false}})),
    }
}
