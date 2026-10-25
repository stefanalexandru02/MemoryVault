//! PreToolUse file context: what earlier sessions learned about a file about to be read.

use std::time::Duration;

use serde_json::{Value, json};

use crate::env::{Out, TestEnv};

fn row(id: &str, title: &str, importance: f64) -> Value {
    json!({"id": id, "table": "observations", "type": "bugfix", "title": title, "created_at": "2026-10-24T10:16:00Z",
           "scope": {"kind": "project", "key": "local/proj"}, "token_estimate": 80, "importance": importance})
}

fn read(env: &TestEnv, file: &str) -> Out {
    let path = env.cwd.join(file);
    env.hook("file-context", &env.input("PreToolUse", json!({"tool_name": "Read", "tool_input": {"file_path": path}})))
}

fn context_of(out: &Out) -> String {
    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string()
}

fn three_rows() -> Vec<Value> {
    vec![
        row("01930000-0000-7000-8000-000000a1b2c3", "Webhook handler retried on every 5xx and double-charged", 0.9),
        row("01930000-0000-7000-8000-000000d4e5f6", "Idempotency key moved from header to body", 0.7),
        row("01930000-0000-7000-8000-000000112233", "Signature check compares in constant time", 0.6),
        row("01930000-0000-7000-8000-000000445566", "Fourth row is never shown", 0.6),
    ]
}

#[test]
fn shows_three_titles_under_a_one_line_header_with_a_project_relative_path() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().by_file = three_rows();
    let out = read(&env, "src/webhooks/payments.rs");
    assert_eq!(out.code, Some(0));
    let block = context_of(&out);
    assert_eq!(
        block,
        "memory: earlier notes on src/webhooks/payments.rs (memory_get id for details)\n\
         - Webhook handler retried on every 5xx and double-charged (a1b2c3)\n\
         - Idempotency key moved from header to body (d4e5f6)\n\
         - Signature check compares in constant time (112233)"
    );
    assert!(block.chars().count().div_ceil(4) <= 110, "{} chars", block.len());
    let req = &fake.matching("GET", "/v1/observations/by-file")[0];
    assert!(req.path.contains("project_key=local%2Fproj") && req.path.ends_with("&limit=8"), "{}", req.path);
}

#[test]
fn speaks_once_per_file_and_not_about_minor_or_unknown_files() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().by_file = three_rows();
    assert!(read(&env, "a.rs").stdout.contains("additionalContext"));
    assert_eq!(read(&env, "a.rs").stdout, "", "the same file again");
    fake.state().by_file = vec![row("01930000-0000-7000-8000-000000a1b2c3", "Renamed a variable", 0.2)];
    assert_eq!(read(&env, "b.rs").stdout, "", "every row minor");
    fake.state().by_file = vec![];
    assert_eq!(read(&env, "c.rs").stdout, "", "nothing known");
    let other = env.hook("file-context", &env.input("PreToolUse", json!({"tool_name": "Bash", "tool_input": {}})));
    assert_eq!(other.stdout, "");
}

#[test]
fn a_session_budget_caps_the_total_and_zero_turns_the_hook_off() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().by_file = three_rows();
    let asked = || fake.matching("GET", "/v1/observations/by-file").len();
    // A block is about 60 estimated tokens: 100 fits one, and the second would pass the cap.
    env.append_config("MEM_FILE_CONTEXT_BUDGET=100");
    assert!(read(&env, "a.rs").stdout.contains("additionalContext"));
    assert_eq!(read(&env, "b.rs").stdout, "");
    assert_eq!(asked(), 2, "the second block was fetched and did not fit");
    // With less than a header and a title left, the server is not even asked.
    env.append_config("MEM_FILE_CONTEXT_BUDGET=80");
    assert_eq!(read(&env, "c.rs").stdout, "");
    assert_eq!(asked(), 2);

    env.append_config("MEM_FILE_CONTEXT_BUDGET=0");
    let before = fake.reqs().len();
    assert_eq!(read(&env, "d.rs").stdout, "");
    assert_eq!(fake.reqs().len(), before, "0 disables the hook before any request");
}

#[test]
fn errors_and_slow_servers_are_silent() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().by_file = three_rows();
    fake.state().down = true;
    let out = read(&env, "a.rs");
    assert_eq!((out.code, out.stdout.as_str(), out.stderr.as_str()), (Some(0), "", ""));
    fake.state().down = false;
    fake.state().reply_delay = Duration::from_secs(3);
    let started = std::time::Instant::now();
    let out = read(&env, "b.rs");
    assert_eq!((out.code, out.stdout.as_str()), (Some(0), ""));
    assert!(started.elapsed() < Duration::from_millis(2500), "gave up after the timeout");
}
