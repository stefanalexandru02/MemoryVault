//! `mem-hook observe` (spec 8.5, 8.6): lease work packets, run `claude -p` on each, post results.
//!
//! `observe --session <id> [--final]` is started by the hooks; `observe --all [--kinds a,b]
//! [--max N]` is the nightly runner.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::api::Api;
use crate::config::{self, Config};
use crate::{deliver::REQUEST_TIMEOUT, util};

const SESSION_KINDS: [&str; 3] = ["extract", "summarize_prompt", "summarize_session"];
const ALL_KINDS: [&str; 6] = [
    "extract",
    "summarize_prompt",
    "summarize_session",
    "consolidate_facts",
    "consolidate_summary",
    "consolidate_procedures",
];
const LEASE_TIMEOUT: Duration = Duration::from_secs(10);
const RESULT_TIMEOUT: Duration = Duration::from_secs(20);
const CLAUDE_TIMEOUT_S: u64 = 180;
/// Linux caps a single argv string at 128 KiB; a longer prompt goes to claude on stdin instead.
const MAX_ARG_PROMPT: usize = 100_000;
const LOG_LIMIT: u64 = 256 * 1024;

#[derive(Debug, PartialEq)]
struct Opts {
    session: Option<String>,
    final_run: bool,
    kinds: Vec<String>,
    max: usize,
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let (mut session, mut final_run, mut all, mut kinds, mut max) = (None, false, false, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--session" => session = Some(value("--session")?),
            "--final" => final_run = true,
            "--all" => all = true,
            "--kinds" => kinds = Some(value("--kinds")?),
            "--max" => max = Some(value("--max")?.parse::<usize>().map_err(|_| "--max needs a number")?),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if session.is_none() && !all {
        return Err("usage: observe --session <id> [--final] | observe --all [--kinds a,b] [--max N]".into());
    }
    let kinds = match kinds {
        Some(k) => k.split(',').map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned).collect(),
        None if session.is_some() => SESSION_KINDS.map(String::from).to_vec(),
        None => ALL_KINDS.map(String::from).to_vec(),
    };
    let max = max.unwrap_or(match (&session, final_run) {
        (None, _) => 50,
        (Some(_), true) => 10,
        (Some(_), false) => 3,
    });
    Ok(Opts { session, final_run, kinds, max })
}

/// Passes one observer process runs at most (its own plus replayed requests).
const MAX_PASSES: usize = 10;

pub fn run(args: &[String]) {
    let opts = match parse_args(args) {
        Ok(o) => o,
        Err(e) => return eprintln!("mem-hook observe: {e}"),
    };
    let home = config::memory_home();
    let Some(cfg) = Config::load(&home) else { return };
    let _ = util::mkdir_private(&home);
    let lock_path = home.join("observe.lock");
    let requests = home.join("observe.again");
    // One observer per machine at a time (the lock is held by the open file). A busy lock leaves
    // this run's arguments for the holder, which replays them before it lets go, so a window that
    // closes while another observer is finishing is not left waiting for the next run.
    let Some(lock) = util::open_lock(&lock_path) else { return };
    if !util::try_lock(&lock) {
        request_again(&requests, args);
        if !util::try_lock(&lock) {
            return;
        }
    }
    let api = Api::new(&cfg);
    let mut lock = Some(lock);
    let mut queue = vec![opts];
    let mut passes = 0;
    while let Some(held) = lock.take() {
        loop {
            for o in std::mem::take(&mut queue) {
                if passes < MAX_PASSES {
                    passes += 1;
                    pass(&home, &api, &cfg, &o);
                }
            }
            queue = take_requests(&requests);
            if queue.is_empty() {
                break;
            }
        }
        drop(held);
        // A request written after the last take but before the unlock: pick it up unless another
        // observer got the lock first (it replays requests itself).
        if passes < MAX_PASSES && requests.exists() {
            lock = util::open_lock(&lock_path).filter(util::try_lock);
        }
    }
}

/// Appends `args` (one JSON array per line) to the request file.
fn request_again(path: &Path, args: &[String]) {
    if let Ok(mut f) = util::open_append(path) {
        let _ = writeln!(f, "{}", json!(args));
    }
}

/// Takes every pending request (rename, then read), deduplicated.
fn take_requests(path: &Path) -> Vec<Opts> {
    let taken = path.with_extension(format!("taken.{}", std::process::id()));
    if std::fs::rename(path, &taken).is_err() {
        return Vec::new();
    }
    let text = std::fs::read_to_string(&taken).unwrap_or_default();
    let _ = std::fs::remove_file(&taken);
    let mut out: Vec<Opts> = Vec::new();
    for line in text.lines() {
        let Ok(args) = serde_json::from_str::<Vec<String>>(line) else { continue };
        if let Ok(o) = parse_args(&args)
            && !out.contains(&o)
        {
            out.push(o);
        }
    }
    out
}

/// Leases and runs up to `opts.max` packets.
fn pass(home: &Path, api: &Api, cfg: &Config, opts: &Opts) {
    log(home, &format!("observer start {opts:?}"));
    let mut done = 0;
    while done < opts.max {
        let lease = json!({ "session_id": opts.session, "kinds": opts.kinds, "max": 1, "runner_kind": "claude-p" });
        let packet = match api.post("/v1/work/lease", &lease, LEASE_TIMEOUT) {
            Ok(r) if r.ok() => r.json().and_then(|v| v["packets"].get(0).cloned()),
            Ok(r) => {
                log(home, &format!("lease failed: HTTP {}", r.status));
                None
            }
            Err(e) => {
                log(home, &format!("lease failed: {e}"));
                None
            }
        };
        let Some(packet) = packet else { break };
        done += 1;
        if !process(cfg, api, &packet) {
            break;
        }
    }
    log(home, &format!("observer done, {done} packet(s), final={}", opts.final_run));
}

fn log(home: &Path, msg: &str) {
    let path = home.join("observe.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = util::open_append(&path) {
        let _ = writeln!(f, "{} {msg}", util::now_rfc3339());
    }
}

/// Why a claude run did not produce a usable answer.
struct Failure {
    kind: &'static str,
    detail: String,
}

/// What the result endpoint said.
enum Posted {
    Applied,
    SchemaInvalid(String),
    Rejected(String),
    Unreachable(String),
}

/// Runs one leased packet. False when the loop should stop (nothing more can succeed now).
fn process(cfg: &Config, api: &Api, packet: &Value) -> bool {
    let Some(id) = packet["id"].as_str() else { return true };
    let system = packet["prompt"]["system"].as_str().unwrap_or_default();
    let user = packet["prompt"]["user"]
        .as_str()
        .filter(|u| !u.is_empty())
        .or_else(|| packet["prompt"]["user_template"].as_str())
        .unwrap_or_default();
    let mut model = packet["model_hint"].as_str().filter(|m| !m.is_empty()).unwrap_or("sonnet").to_string();
    let mut prompt = user.to_string();
    let mut retried = false;
    loop {
        let reply = match run_claude(cfg, &model, system, &prompt) {
            Ok(r) => r,
            Err(f) => {
                log(&cfg.home, &format!("packet {id}: claude failed ({}): {}", f.kind, f.detail));
                post_fail(api, id, &f);
                return !matches!(f.kind, "quota_exhausted" | "cli_missing");
            }
        };
        let meta = json!({
            "model": reply.model.clone().unwrap_or_else(|| model.clone()),
            "duration_ms": reply.duration_ms,
            "cost_usd": reply.cost_usd,
            "runner_kind": "claude-p",
            "runner": cfg.device,
        });
        match post_result(api, id, &reply.text, &meta) {
            Posted::Applied => return true,
            Posted::SchemaInvalid(why) if !retried => {
                // One retry on Opus with the validation error appended to the prompt.
                log(&cfg.home, &format!("packet {id}: schema_invalid, retrying with opus: {why}"));
                retried = true;
                model = "opus".into();
                prompt = format!("{user}\n\n{why}");
            }
            Posted::SchemaInvalid(why) | Posted::Rejected(why) => {
                log(&cfg.home, &format!("packet {id}: result rejected: {why}"));
                post_fail(api, id, &Failure { kind: "other", detail: why });
                return true;
            }
            Posted::Unreachable(why) => {
                log(&cfg.home, &format!("packet {id}: result not delivered: {why}"));
                return false;
            }
        }
    }
}

fn post_fail(api: &Api, id: &str, f: &Failure) {
    let detail: String = f.detail.chars().take(1000).collect();
    let _ =
        api.post(&format!("/v1/work/{id}/fail"), &json!({ "error": f.kind, "detail": detail }), REQUEST_TIMEOUT * 5);
}

fn post_result(api: &Api, id: &str, text: &str, meta: &Value) -> Posted {
    // The output must be a JSON object. Send it parsed when it is; otherwise send the raw text and
    // let the server strip a code fence or answer 422.
    let output = match serde_json::from_str::<Value>(text.trim()) {
        Ok(v) if v.is_object() => v,
        _ => Value::String(text.to_string()),
    };
    let body = json!({ "output": output, "meta": meta });
    match api.post(&format!("/v1/work/{id}/result"), &body, RESULT_TIMEOUT) {
        Err(e) => Posted::Unreachable(e),
        Ok(r) if r.ok() => Posted::Applied,
        Ok(r) => {
            let error = r.json().map(|v| v["error"].clone()).unwrap_or(Value::Null);
            if r.status == 422 && error["code"] == "schema_invalid" {
                let why =
                    error["retry_with"].as_str().or(error["message"].as_str()).unwrap_or("schema validation failed");
                return Posted::SchemaInvalid(why.to_string());
            }
            Posted::Rejected(format!("HTTP {}: {}", r.status, r.body.chars().take(300).collect::<String>()))
        }
    }
}

struct Reply {
    text: String,
    cost_usd: Option<f64>,
    duration_ms: Option<i64>,
    model: Option<String>,
}

/// Runs `claude -p` in a fresh temp dir with the observer guards (spec 8.5).
fn run_claude(cfg: &Config, model: &str, system: &str, user: &str) -> Result<Reply, Failure> {
    let timeout =
        std::env::var("MEMORY_OBSERVE_TIMEOUT_S").ok().and_then(|v| v.parse().ok()).unwrap_or(CLAUDE_TIMEOUT_S);
    let dir = scratch_dir().map_err(|e| Failure { kind: "other", detail: format!("temp dir: {e}") })?;
    let out = spawn_claude(cfg, &dir, model, system, user, Duration::from_secs(timeout));
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.map_err(|e| {
        let kind = if e.kind() == std::io::ErrorKind::NotFound { "cli_missing" } else { "other" };
        Failure { kind, detail: format!("cannot run {}: {e}", cfg.claude_bin) }
    })?;
    let reply = parse_reply(&out.stdout);
    if out.timed_out {
        return Err(Failure { kind: "timeout", detail: format!("claude -p exceeded {timeout}s") });
    }
    let failed = out.code != Some(0) || reply.as_ref().is_some_and(|r| r.is_error);
    if failed {
        let text = reply.map(|r| r.text).unwrap_or_default();
        let kind = classify(out.code, &out.stderr, &text);
        let detail = format!("exit {:?}: {}{}", out.code, out.stderr.trim(), text);
        return Err(Failure { kind, detail });
    }
    match reply {
        Some(r) => Ok(Reply { text: r.text, cost_usd: r.cost_usd, duration_ms: r.duration_ms, model: r.model }),
        None => Err(Failure {
            kind: "other",
            detail: format!("unparseable claude output: {}", out.stdout.chars().take(300).collect::<String>()),
        }),
    }
}

fn scratch_dir() -> std::io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let dir = std::env::temp_dir().join(format!("mem-observe-{}-{nanos}", std::process::id()));
    util::mkdir_private(&dir)?;
    Ok(dir)
}

struct RawOutput {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    timed_out: bool,
}

fn spawn_claude(
    cfg: &Config,
    dir: &Path,
    model: &str,
    system: &str,
    user: &str,
    timeout: Duration,
) -> std::io::Result<RawOutput> {
    let mut cmd = Command::new(&cfg.claude_bin);
    cmd.args(["-p", "--model", model, "--output-format", "json", "--disallowedTools", "*", "--no-session-persistence"])
        .args(["--max-turns", "1", "--settings", r#"{"disableAllHooks": true}"#, "--system-prompt", system]);
    let via_stdin = user.len() > MAX_ARG_PROMPT;
    if !via_stdin {
        cmd.arg(user);
    }
    cmd.current_dir(dir)
        .env("MEMORY_OBSERVER", "1")
        .stdin(if via_stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    util::own_process_group(&mut cmd);
    let mut child = cmd.spawn()?;
    let feeder = child.stdin.take().map(|mut stdin| {
        let text = user.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        })
    });
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(mut p) = pipe {
                let mut bytes = Vec::new();
                let _ = p.read_to_end(&mut bytes);
                buf = String::from_utf8_lossy(&bytes).into_owned();
            }
            buf
        })
    };
    let out_thread = reader(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err_thread = reader(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (Some(status), false);
        }
        if Instant::now() >= deadline {
            util::kill_tree(&mut child);
            break (child.wait().ok(), true);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if let Some(f) = feeder {
        let _ = f.join();
    }
    Ok(RawOutput {
        stdout: out_thread.join().unwrap_or_default(),
        stderr: err_thread.join().unwrap_or_default(),
        code: status.and_then(|s| s.code()),
        timed_out,
    })
}

struct Parsed {
    text: String,
    is_error: bool,
    cost_usd: Option<f64>,
    duration_ms: Option<i64>,
    model: Option<String>,
}

/// `claude -p --output-format json` prints one result object (older versions: an array of events).
fn parse_reply(stdout: &str) -> Option<Parsed> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let obj = match &v {
        Value::Array(items) => items.iter().rev().find(|i| i["type"] == "result")?,
        other => other,
    };
    Some(Parsed {
        text: obj["result"].as_str()?.to_string(),
        is_error: obj["is_error"].as_bool().unwrap_or(false),
        cost_usd: obj["total_cost_usd"].as_f64(),
        duration_ms: obj["duration_ms"].as_i64(),
        model: obj["modelUsage"].as_object().and_then(|m| m.keys().next().cloned()),
    })
}

/// Maps a failed run to the server's fail reasons.
fn classify(code: Option<i32>, stderr: &str, text: &str) -> &'static str {
    if code == Some(127) {
        return "cli_missing";
    }
    let haystack = format!("{stderr}\n{text}").to_lowercase();
    const QUOTA: [&str; 7] =
        ["usage limit", "rate limit", "rate_limit", "quota", "credit balance", "too many requests", "429"];
    if QUOTA.iter().any(|p| haystack.contains(p)) { "quota_exhausted" } else { "other" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_session_and_nightly_args() {
        let o = parse_args(&args("--session abc --final")).unwrap();
        assert_eq!((o.session.as_deref(), o.final_run, o.max), (Some("abc"), true, 10));
        assert_eq!(o.kinds, SESSION_KINDS);
        let o = parse_args(&args("--all --kinds extract,consolidate_facts --max 7")).unwrap();
        assert_eq!((o.session, o.max, o.kinds), (None, 7, vec!["extract".into(), "consolidate_facts".into()]));
        assert_eq!(parse_args(&args("--all")).unwrap().max, 50);
        assert!(parse_args(&args("--final")).is_err());
        assert!(parse_args(&args("--session")).is_err());
    }

    #[test]
    fn classifies_failures() {
        assert_eq!(classify(Some(1), "Claude AI usage limit reached", ""), "quota_exhausted");
        assert_eq!(classify(Some(127), "", ""), "cli_missing");
        assert_eq!(classify(Some(1), "boom", ""), "other");
    }

    #[test]
    fn parses_claude_json() {
        let p = parse_reply(r#"{"type":"result","is_error":false,"result":"{}","total_cost_usd":0.5,"duration_ms":10,"modelUsage":{"claude-sonnet":{}}}"#).unwrap();
        assert_eq!(
            (p.text.as_str(), p.cost_usd, p.duration_ms, p.model.as_deref()),
            ("{}", Some(0.5), Some(10), Some("claude-sonnet"))
        );
        assert!(parse_reply(r#"[{"type":"system"},{"type":"result","result":"x"}]"#).is_some());
        assert!(parse_reply("not json").is_none());
    }
}
