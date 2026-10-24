//! The observer: spawning from hooks, lease/result/fail flow, claude flags, retries, locking.

use std::os::unix::io::AsRawFd;

use serde_json::{Value, json};

use crate::env::{TestEnv, claude_ok};
use crate::fake_server::{Fake, SESSION_ID, packet};

const OBSERVATION: &str = r#"{"observations":[]}"#;

fn setup() -> (Fake, TestEnv) {
    let (fake, env) = TestEnv::with_fake();
    env.install_claude(&claude_ok(OBSERVATION));
    (fake, env)
}

fn results(fake: &Fake) -> Vec<crate::fake_server::Req> {
    fake.matching("POST", "/v1/work/").into_iter().filter(|r| r.path.ends_with("/result")).collect()
}

fn fails(fake: &Fake) -> Vec<crate::fake_server::Req> {
    fake.matching("POST", "/v1/work/").into_iter().filter(|r| r.path.ends_with("/fail")).collect()
}

fn leases(fake: &Fake) -> Vec<Value> {
    fake.matching("POST", "/v1/work/lease").iter().map(|r| r.body.clone()).collect()
}

#[test]
fn window_closed_spawns_the_observer_which_runs_the_lease_result_flow() {
    let (fake, env) = setup();
    fake.state().window_closed = true;
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    let out = env
        .hook("tool", &env.input("PostToolUse", json!({"tool_name": "Edit", "tool_input": {}, "tool_response": {}})));
    assert_eq!((out.code, out.stdout.as_str()), (Some(0), ""));
    fake.wait_for("result post", |r| r.iter().any(|q| q.path == "/v1/work/pkt-1/result"));

    let lease = &leases(&fake)[0];
    assert_eq!(
        lease,
        &json!({"session_id": SESSION_ID, "kinds": ["extract", "summarize_prompt", "summarize_session"], "max": 1})
    );
    let result = &results(&fake)[0];
    assert_eq!(result.body["output"], json!({"observations": []}));
    assert_eq!(
        result.body["meta"],
        json!({"model": "claude-sonnet-4", "duration_ms": 8400, "cost_usd": 0.012, "runner_kind": "claude-p", "runner": "laptop-x1"})
    );
    assert_eq!(result.header("authorization"), Some("Bearer mem_abcd1234_secret"));
    assert!(fails(&fake).is_empty());
    // The observer asked for more work after the packet and found none.
    fake.wait_for("second lease", |r| r.iter().filter(|q| q.path == "/v1/work/lease").count() == 2);
}

#[test]
fn window_not_closed_does_not_spawn() {
    let (fake, env) = setup();
    env.hook("tool", &env.input("PostToolUse", json!({"tool_name": "Edit"})));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(fake.matching("POST", "/v1/work/lease").is_empty());
}

#[test]
fn claude_gets_the_exact_flags_environment_and_a_fresh_directory() {
    let (fake, env) = setup();
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    let out = env.observe(&["--session", SESSION_ID]);
    assert_eq!(out.code, Some(0));
    assert_eq!(env.claude_calls(), 1);
    assert_eq!(
        env.claude_args(1),
        [
            "-p",
            "--model",
            "sonnet",
            "--output-format",
            "json",
            "--disallowedTools",
            "*",
            "--no-session-persistence",
            "--max-turns",
            "1",
            "--settings",
            r#"{"disableAllHooks": true}"#,
            "--system-prompt",
            "SYSTEM PROMPT",
            "USER PROMPT\nline two",
        ]
    );
    assert_eq!(env.claude_file(1, "observer"), "1");
    let pwd = env.claude_file(1, "pwd");
    let pwd = pwd.trim();
    assert!(
        !pwd.is_empty() && !pwd.starts_with(env.cwd.to_str().unwrap()) && pwd != env.home.to_str().unwrap(),
        "{pwd}"
    );
    assert!(!std::path::Path::new(pwd).exists(), "scratch dir is removed");
}

#[test]
fn model_hint_is_passed_through() {
    let (fake, env) = setup();
    fake.queue_packet(packet("pkt-1", "consolidate_facts", "opus"));
    env.observe(&["--all"]);
    assert_eq!(env.claude_args(1)[2], "opus");
}

#[test]
fn schema_invalid_retries_once_with_opus_and_the_error_appended() {
    let (fake, env) = setup();
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    fake.state().result_replies.push_back((
        422,
        json!({"error": {"code": "schema_invalid", "message": "bad", "retryable": false, "retry_with": "output does not match observer.v1: /observations: missing"}}),
    ));
    env.claude_script(2, "reply", &claude_ok(r#"{"observations":[{"title":"fixed"}]}"#));
    env.observe(&["--session", SESSION_ID]);

    assert_eq!(env.claude_calls(), 2);
    let first = env.claude_args(1);
    let second = env.claude_args(2);
    assert_eq!(first[2], "sonnet");
    assert_eq!(second[2], "opus");
    let user = second.last().unwrap();
    assert!(user.starts_with("USER PROMPT\nline two"), "{user}");
    assert!(user.ends_with("output does not match observer.v1: /observations: missing"), "{user}");
    // Everything else is unchanged.
    assert_eq!(first[..first.len() - 1].iter().enumerate().filter(|(i, _)| *i != 2).count(), second.len() - 2);
    let posted = results(&fake);
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[1].body["output"], json!({"observations": [{"title": "fixed"}]}));
    assert!(fails(&fake).is_empty());
}

#[test]
fn second_schema_invalid_fails_the_packet() {
    let (fake, env) = setup();
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    for _ in 0..2 {
        fake.state().result_replies.push_back((
            422,
            json!({"error": {"code": "schema_invalid", "message": "bad", "retryable": false, "retry_with": "still wrong"}}),
        ));
    }
    env.observe(&["--session", SESSION_ID]);
    assert_eq!(env.claude_calls(), 2);
    let f = fails(&fake);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].path, "/v1/work/pkt-1/fail");
    assert_eq!(f[0].body["error"], "other");
    assert!(f[0].body["detail"].as_str().unwrap().contains("still wrong"));
}

#[test]
fn fenced_text_is_sent_raw_for_the_server_to_unwrap() {
    let (fake, env) = setup();
    let fenced = "```json\n{\"observations\": []}\n```";
    env.install_claude(&claude_ok(fenced));
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    env.observe(&["--session", SESSION_ID]);
    assert_eq!(results(&fake)[0].body["output"], fenced);
}

#[test]
fn quota_failure_is_classified_and_stops_the_loop() {
    let (fake, env) = setup();
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    fake.queue_packet(packet("pkt-2", "extract", "sonnet"));
    env.claude_script(1, "exit", "1");
    env.claude_script(1, "stderr", "Claude AI usage limit reached|1700000000");
    env.observe(&["--session", SESSION_ID]);
    let f = fails(&fake);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].body["error"], "quota_exhausted");
    assert!(f[0].body["detail"].as_str().unwrap().contains("usage limit"));
    assert_eq!(leases(&fake).len(), 1, "no second lease after quota exhaustion");
    assert!(results(&fake).is_empty());
}

#[test]
fn other_failure_continues_with_the_next_packet() {
    let (fake, env) = setup();
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    fake.queue_packet(packet("pkt-2", "extract", "sonnet"));
    env.claude_script(1, "exit", "2");
    env.claude_script(1, "stderr", "something odd");
    env.observe(&["--session", SESSION_ID]);
    assert_eq!(fails(&fake)[0].body["error"], "other");
    assert_eq!(results(&fake)[0].path, "/v1/work/pkt-2/result");
}

#[test]
fn missing_cli_is_cli_missing() {
    let (fake, env) = TestEnv::with_fake();
    std::fs::write(
        env.home.join("config"),
        format!("MEMORY_URL={}\nMEMORY_TOKEN=t\nCLAUDE_BIN=/nonexistent/claude\n", fake.url),
    )
    .unwrap();
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    env.observe(&["--session", SESSION_ID]);
    assert_eq!(fails(&fake)[0].body["error"], "cli_missing");
}

#[test]
fn claude_timeout_is_classified() {
    let (fake, mut env) = setup();
    env.extra_env.push(("MEMORY_OBSERVE_TIMEOUT_S".into(), "1".into()));
    env.claude_script(1, "sleep", "30");
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    let started = std::time::Instant::now();
    env.observe(&["--session", SESSION_ID]);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(fails(&fake)[0].body["error"], "timeout");
}

#[test]
fn claude_error_result_is_a_failure() {
    let (fake, env) = setup();
    env.install_claude(&json!({"type": "result", "is_error": true, "result": "API Error: 429 rate limit"}).to_string());
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    env.observe(&["--session", SESSION_ID]);
    assert_eq!(fails(&fake)[0].body["error"], "quota_exhausted");
    assert!(results(&fake).is_empty());
}

#[test]
fn garbage_output_is_a_failure() {
    let (fake, env) = setup();
    env.install_claude("not json at all");
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    env.observe(&["--session", SESSION_ID]);
    assert_eq!(fails(&fake)[0].body["error"], "other");
}

#[test]
fn session_runs_stop_after_three_packets() {
    let (fake, env) = setup();
    for i in 0..5 {
        fake.queue_packet(packet(&format!("pkt-{i}"), "extract", "sonnet"));
    }
    env.observe(&["--session", SESSION_ID]);
    assert_eq!(results(&fake).len(), 3);
    assert_eq!(leases(&fake).len(), 3);
}

#[test]
fn nightly_runner_leases_without_a_session_and_honours_kinds_and_max() {
    let (fake, env) = setup();
    for i in 0..4 {
        fake.queue_packet(packet(&format!("pkt-{i}"), "consolidate_facts", "opus"));
    }
    let out = env.observe(&["--all", "--kinds", "consolidate_facts,consolidate_summary", "--max", "2"]);
    assert_eq!(out.code, Some(0));
    let l = leases(&fake);
    assert_eq!(l.len(), 2);
    assert_eq!(l[0], json!({"session_id": null, "kinds": ["consolidate_facts", "consolidate_summary"], "max": 1}));
    assert_eq!(results(&fake).len(), 2);
}

#[test]
fn nightly_runner_defaults_to_every_kind() {
    let (fake, env) = setup();
    env.observe(&["--all"]);
    assert_eq!(
        leases(&fake)[0]["kinds"],
        json!([
            "extract",
            "summarize_prompt",
            "summarize_session",
            "consolidate_facts",
            "consolidate_summary",
            "consolidate_procedures"
        ])
    );
}

#[test]
fn only_one_observer_per_machine() {
    let (fake, env) = setup();
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    let lock = std::fs::OpenOptions::new().create(true).append(true).open(env.home.join("observe.lock")).unwrap();
    // SAFETY: plain flock on a descriptor we own.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let out = env.observe(&["--session", SESSION_ID]);
    assert_eq!(out.code, Some(0));
    assert!(fake.reqs().is_empty());
    // The busy run left its arguments for the lock holder.
    assert!(env.home.join("observe.again").exists());
    drop(lock);
    env.observe(&["--session", SESSION_ID]);
    // Own pass: the packet, then an empty lease; then the replayed request: one more empty lease.
    assert_eq!(leases(&fake).len(), 3);
    assert_eq!(results(&fake).len(), 1);
    assert!(!env.home.join("observe.again").exists());
}

#[test]
fn a_run_refused_by_the_lock_is_replayed_by_the_holder() {
    let (fake, env) = setup();
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    env.claude_script(1, "sleep", "1");
    std::thread::scope(|s| {
        // The holder takes one packet only, so without the replay pkt-2 would wait for the next run.
        let holder = s.spawn(|| env.observe(&["--session", SESSION_ID, "--max", "1"]));
        fake.wait_for("first lease", |r| r.iter().any(|q| q.path == "/v1/work/lease"));
        fake.queue_packet(packet("pkt-2", "summarize_prompt", "sonnet"));
        let busy = env.observe(&["--session", SESSION_ID]);
        assert_eq!(busy.code, Some(0));
        holder.join().unwrap();
    });
    let posted: Vec<String> = results(&fake).iter().map(|r| r.path.clone()).collect();
    assert_eq!(posted, vec!["/v1/work/pkt-1/result", "/v1/work/pkt-2/result"]);
}

#[test]
fn stop_spawns_the_observer() {
    let (fake, env) = setup();
    fake.queue_packet(packet("pkt-1", "summarize_prompt", "sonnet"));
    env.hook("stop", &env.input("Stop", json!({"last_assistant_message": "done", "stop_hook_active": false})));
    fake.wait_for("result post", |r| r.iter().any(|q| q.path == "/v1/work/pkt-1/result"));
    assert_eq!(leases(&fake)[0]["session_id"], SESSION_ID);
}

#[test]
fn session_end_spawns_the_final_observer() {
    let (fake, env) = setup();
    fake.queue_packet(packet("pkt-1", "summarize_session", "sonnet"));
    env.hook("session-end", &env.input("SessionEnd", json!({"reason": "other"})));
    fake.wait_for("result post", |r| r.iter().any(|q| q.path == "/v1/work/pkt-1/result"));
}

#[test]
fn observer_without_credentials_does_nothing() {
    let (fake, env) = setup();
    std::fs::remove_file(env.home.join("config")).unwrap();
    let out = env.observe(&["--all"]);
    assert_eq!(out.code, Some(0));
    assert!(fake.reqs().is_empty());
}

#[test]
fn bad_arguments_exit_zero() {
    let (fake, env) = setup();
    let out = env.observe(&["--bogus"]);
    assert_eq!(out.code, Some(0));
    assert!(out.stderr.contains("unknown argument"));
    assert!(fake.reqs().is_empty());
}

#[test]
fn oversized_prompts_go_to_claude_on_stdin() {
    let (fake, env) = setup();
    let mut p = packet("pkt-1", "extract", "sonnet");
    let big = "x".repeat(150_000);
    p["prompt"]["user"] = json!(big);
    fake.queue_packet(p);
    env.observe(&["--session", SESSION_ID]);
    let args = env.claude_args(1);
    assert_eq!(args.last().unwrap(), "SYSTEM PROMPT");
    assert_eq!(env.claude_file(1, "stdin"), big);
    assert_eq!(results(&fake).len(), 1);
}
