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

const BLOCK: &str = "memory: earlier work that may matter for this prompt (memory_get id for details)\n\
                     | id | time | t | title | ~tok |\n|---|---|---|---|---|\n**2026-10-24**\n\
                     | a1b2c3 | 10:16 | ● | Webhook retries double-charged | 120 |\n_Legend: ● bugfix_";

fn smart_env() -> (crate::fake_server::Fake, TestEnv) {
    let (fake, env) = TestEnv::with_fake();
    {
        let mut state = fake.state();
        state.context = Some("## Memory\n### Profile\n- Alex\n".into());
        state.injection = Some("smart".into());
        state.prompt_context = Some(BLOCK.into());
    }
    env.append_config("MEM_INJECTION=smart");
    (fake, env)
}

fn start(env: &TestEnv) {
    env.hook("session-start", &env.input("SessionStart", json!({"source": "startup", "model": "test-model"})));
}

fn prompt(env: &TestEnv, text: &str) -> Out {
    env.hook("prompt", &env.input("UserPromptSubmit", json!({"prompt": text})))
}

#[test]
fn smart_mode_adds_memories_to_the_first_prompt_only() {
    let (fake, env) = smart_env();
    start(&env);
    assert_eq!(fake.matching("GET", "/v1/context")[0].path.split("mode=").nth(1), Some("smart"));
    let long = format!("fix the webhook retries {}", "x".repeat(800));
    let out = prompt(&env, &long);
    assert_eq!(out.code, Some(0));
    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert_eq!(v["hookSpecificOutput"]["additionalContext"], BLOCK);

    // What went to the server: the start of the prompt, the project and the device.
    let asked = fake.matching("POST", "/v1/context/prompt");
    assert_eq!(asked.len(), 1);
    let sent = asked[0].body["prompt"].as_str().unwrap();
    assert!(sent.chars().count() == 500 && long.starts_with(sent), "{}", sent.len());
    assert_eq!(
        (asked[0].body["project_key"].as_str(), asked[0].body["device_id"].as_str()),
        (Some("local/proj"), Some("laptop-x1"))
    );
    // The prompt is recorded as ever.
    assert_eq!(fake.events().iter().filter(|e| e["kind"] == "user_prompt").count(), 1);

    // The second prompt is not asked about.
    assert_eq!(prompt(&env, "and the idempotency table?").stdout, "");
    assert_eq!(fake.matching("POST", "/v1/context/prompt").len(), 1);
}

#[test]
fn a_prompt_nothing_matches_adds_nothing_but_still_uses_up_the_first_prompt() {
    let (fake, env) = smart_env();
    fake.state().prompt_context = Some(String::new());
    start(&env);
    assert_eq!(prompt(&env, "what is the weather").stdout, "");
    fake.state().prompt_context = Some(BLOCK.into());
    assert_eq!(prompt(&env, "fix the webhook retries").stdout, "");
    assert_eq!(fake.matching("POST", "/v1/context/prompt").len(), 1);
}

#[test]
fn other_modes_and_an_unknown_mode_never_ask() {
    for mode in [Some("lean"), Some("full"), None] {
        let (fake, env) = smart_env();
        fake.state().injection = mode.map(String::from);
        start(&env);
        assert_eq!(prompt(&env, "fix the webhook retries").stdout, "", "{mode:?}");
        assert!(fake.matching("POST", "/v1/context/prompt").is_empty(), "{mode:?}");
    }
    // The server unreachable at session start: no header, so no smart prompt.
    let (fake, env) = smart_env();
    fake.state().down = true;
    start(&env);
    fake.state().down = false;
    assert_eq!(prompt(&env, "fix the webhook retries").stdout, "");
    assert!(fake.matching("POST", "/v1/context/prompt").is_empty());
}

#[test]
fn a_server_default_of_smart_works_without_a_config_setting() {
    let (fake, env) = TestEnv::with_fake();
    {
        let mut state = fake.state();
        state.context = Some("## Memory\n".into());
        state.injection = Some("smart".into());
        state.prompt_context = Some(BLOCK.into());
    }
    start(&env);
    assert!(!fake.matching("GET", "/v1/context")[0].path.contains("mode="));
    assert!(prompt(&env, "fix the webhook retries").stdout.contains(BLOCK.lines().next().unwrap()));
}

#[test]
fn errors_timeouts_and_oversized_answers_are_silent() {
    let (fake, env) = smart_env();
    start(&env);
    fake.state().prompt_context = None; // 404
    let out = prompt(&env, "fix the webhook retries");
    assert_eq!((out.code, out.stdout.as_str(), out.stderr.as_str()), (Some(0), "", ""));

    let (fake, env) = smart_env();
    start(&env);
    fake.state().reply_delay = Duration::from_secs(3);
    fake.state().delayed = "/v1/context/prompt".into();
    let started = std::time::Instant::now();
    let out = prompt(&env, "fix the webhook retries");
    assert_eq!((out.code, out.stdout.as_str()), (Some(0), ""));
    assert!(started.elapsed() < Duration::from_millis(2000), "gave up after 800 ms: {:?}", started.elapsed());
    // The prompt itself was still recorded.
    assert_eq!(fake.events().iter().filter(|e| e["kind"] == "user_prompt").count(), 1);

    let (fake, env) = smart_env();
    start(&env);
    fake.state().prompt_context = Some("x".repeat(1100));
    assert_eq!(prompt(&env, "fix the webhook retries").stdout, "", "over 250 estimated tokens");
}
