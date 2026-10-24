//! Event posting, session cache, project keys, exclude list, config and recursion guard.

use std::process::Command;

use serde_json::json;

use crate::env::{SESSION, TestEnv};
use crate::fake_server::SESSION_ID;

#[test]
fn tool_event_creates_session_then_posts_event() {
    let (fake, env) = TestEnv::with_fake();
    let input = env.input(
        "PostToolUse",
        json!({"tool_name": "Edit", "tool_input": {"file_path": "a.rs"}, "tool_response": {"ok": true},
               "tool_use_id": "toolu_1", "duration_ms": 120, "agent_id": null}),
    );
    let out = env.hook("tool", &input);
    assert_eq!(out.code, Some(0));
    assert_eq!(out.stdout, "");

    let reqs = fake.reqs();
    assert_eq!(reqs.len(), 2, "{reqs:#?}");
    assert_eq!((reqs[0].method.as_str(), reqs[0].path.as_str()), ("POST", "/v1/sessions"));
    assert_eq!(reqs[0].header("authorization"), Some("Bearer mem_abcd1234_secret"));
    assert_eq!(reqs[0].header("x-memory-device"), Some("laptop-x1"));
    assert_eq!(reqs[0].body["harness"], "claude-code");
    assert_eq!(reqs[0].body["external_session_id"], SESSION);
    assert_eq!(reqs[0].body["project_key"], "local/proj");
    assert_eq!(reqs[0].body["cwd"], env.cwd.to_string_lossy().as_ref());

    assert_eq!(reqs[1].path, format!("/v1/sessions/{SESSION_ID}/events"));
    let ev = &reqs[1].body["events"][0];
    assert_eq!((ev["seq"].as_i64(), ev["kind"].as_str()), (Some(0), Some("tool_use")));
    assert_eq!(ev["idempotency_key"], "sess-1:0");
    assert!(ev["occurred_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(ev["payload"]["tool_name"], "Edit");
    assert_eq!(ev["payload"]["tool_input"]["file_path"], "a.rs");
    assert_eq!(ev["payload"]["tool_response"]["ok"], true);
    assert_eq!(ev["payload"]["tool_use_id"], "toolu_1");
    assert_eq!(ev["payload"]["duration_ms"], 120);
    assert!(ev["payload"]["agent_id"].is_null());

    // The cache holds the server session: the next event does not create it again.
    env.hook("tool", &input);
    assert_eq!(fake.matching("POST", "/v1/sessions").iter().filter(|r| r.path == "/v1/sessions").count(), 1);
    let evs = fake.events();
    assert_eq!(evs.iter().map(|e| e["seq"].as_i64().unwrap()).collect::<Vec<_>>(), [0, 1]);
}

#[test]
fn prompt_failure_stop_and_subagent_payloads() {
    let (fake, env) = TestEnv::with_fake();
    env.hook("prompt", &env.input("UserPromptSubmit", json!({"prompt": "fix the bug"})));
    env.hook(
        "tool-failure",
        &env.input(
            "PostToolUseFailure",
            json!({"tool_name": "Bash", "tool_input": {"command": "false"}, "tool_use_id": "t2",
                                                  "error": "exit 1", "is_interrupt": false}),
        ),
    );
    env.hook(
        "subagent-stop",
        &env.input(
            "SubagentStop",
            json!({"agent_id": "a1", "agent_type": "Explore", "last_assistant_message": "done"}),
        ),
    );
    env.hook("stop", &env.input("Stop", json!({"last_assistant_message": "all fixed", "stop_hook_active": false})));

    let evs = fake.events();
    let kinds: Vec<_> = evs.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["user_prompt", "tool_failure", "subagent_stop", "assistant_message"]);
    assert_eq!(evs[0]["payload"], json!({"prompt": "fix the bug"}));
    assert_eq!(evs[1]["payload"]["error"], "exit 1");
    assert_eq!(evs[1]["payload"]["is_interrupt"], false);
    assert_eq!(evs[1]["payload"]["tool_name"], "Bash");
    assert_eq!(evs[2]["payload"], json!({"agent_id": "a1", "agent_type": "Explore", "last_assistant_message": "done"}));
    assert_eq!(evs[3]["payload"], json!({"last_assistant_message": "all fixed", "stop_hook_active": false}));
    let cache: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.home.join("sessions").join(SESSION)).unwrap()).unwrap();
    assert_eq!((cache["prompt_no"].as_i64(), cache["next_seq"].as_i64()), (Some(1), Some(4)));
    assert_eq!(cache["session_id"], SESSION_ID);
}

#[test]
fn stop_with_stop_hook_active_posts_nothing() {
    let (fake, env) = TestEnv::with_fake();
    let out = env.hook("stop", &env.input("Stop", json!({"last_assistant_message": "x", "stop_hook_active": true})));
    assert_eq!(out.code, Some(0));
    assert!(fake.reqs().is_empty());
}

#[test]
fn session_end_posts_end_with_reason() {
    let (fake, env) = TestEnv::with_fake();
    let out = env.hook("session-end", &env.input("SessionEnd", json!({"reason": "prompt_input_exit"})));
    assert_eq!(out.code, Some(0));
    let ends = fake.matching("POST", &format!("/v1/sessions/{SESSION_ID}/end"));
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].body, json!({"reason": "prompt_input_exit"}));
}

#[test]
fn session_end_respects_its_one_second_budget() {
    // A server that never answers: the hook must still return quickly.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let env = TestEnv::new(&format!("http://{}", listener.local_addr().unwrap()));
    let started = std::time::Instant::now();
    let out = env.hook("session-end", &env.input("SessionEnd", json!({"reason": "other"})));
    assert_eq!(out.code, Some(0));
    assert!(started.elapsed() < std::time::Duration::from_millis(1800), "took {:?}", started.elapsed());
    let spooled = env.spool_lines();
    assert_eq!(spooled.len(), 1);
    assert_eq!(spooled[0]["op"], "end");
    drop(listener);
}

#[test]
fn recursion_guard_does_nothing() {
    let (fake, env) = TestEnv::with_fake();
    for event in ["session-start", "prompt", "tool", "stop", "session-end"] {
        let out = env.hook_with_env(
            event,
            &env.input("X", json!({"prompt": "p", "source": "startup"})),
            "MEMORY_OBSERVER",
            "1",
        );
        assert_eq!((out.code, out.stdout.as_str()), (Some(0), ""), "{event}");
    }
    assert!(fake.reqs().is_empty());
    assert!(!env.home.join("sessions").exists() && !env.home.join("spool").exists());
}

#[test]
fn guard_also_blocks_observe() {
    let (fake, env) = TestEnv::with_fake();
    fake.queue_packet(crate::fake_server::packet("p1", "extract", "sonnet"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mem-hook"));
    let status =
        cmd.args(["observe", "--all"]).env("MEMORY_HOME", &env.home).env("MEMORY_OBSERVER", "1").status().unwrap();
    assert!(status.success());
    assert!(fake.reqs().is_empty());
}

#[test]
fn missing_config_is_silent_and_hints_once_a_day_on_session_start() {
    let (fake, env) = TestEnv::with_fake();
    std::fs::remove_file(env.home.join("config")).unwrap();
    let out = env.hook("tool", &env.input("PostToolUse", json!({"tool_name": "Bash"})));
    assert_eq!((out.code, out.stdout.as_str(), out.stderr.as_str()), (Some(0), "", ""));
    let start = env.input("SessionStart", json!({"source": "startup"}));
    let first = env.hook("session-start", &start);
    assert_eq!(first.code, Some(0));
    assert_eq!(first.stdout, "");
    assert!(first.stderr.contains("MEMORY_URL"), "{}", first.stderr);
    assert_eq!(env.hook("session-start", &start).stderr, "");
    assert!(fake.reqs().is_empty());
}

#[test]
fn token_missing_from_config_disables_hooks() {
    let (fake, env) = TestEnv::with_fake();
    std::fs::write(env.home.join("config"), format!("MEMORY_URL={}\n", fake.url)).unwrap();
    env.hook("tool", &env.input("PostToolUse", json!({"tool_name": "Bash"})));
    assert!(fake.reqs().is_empty());
}

#[test]
fn excluded_project_is_ignored() {
    let (fake, env) = TestEnv::with_fake();
    std::fs::write(env.home.join("exclude"), "# private\nlocal/other\nlocal/proj\n").unwrap();
    let out = env.hook("tool", &env.input("PostToolUse", json!({"tool_name": "Bash"})));
    assert_eq!(out.code, Some(0));
    env.hook("session-start", &env.input("SessionStart", json!({"source": "startup"})));
    assert!(fake.reqs().is_empty());
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t", "-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn project_key_sent(fake: &crate::fake_server::Fake) -> String {
    fake.matching("POST", "/v1/sessions")[0].body["project_key"].as_str().unwrap().to_string()
}

#[test]
fn project_key_from_origin_remote() {
    let (fake, env) = TestEnv::with_fake();
    git(&env.cwd, &["init", "-q"]);
    git(&env.cwd, &["remote", "add", "origin", "git@GitHub.com:Alex/Homelab.git"]);
    let sub = env.cwd.join("deep");
    std::fs::create_dir_all(&sub).unwrap();
    let input = env.input("PostToolUse", json!({"tool_name": "Bash", "cwd": sub}));
    env.hook("tool", &input);
    assert_eq!(project_key_sent(&fake), "github.com/alex/homelab");
}

#[test]
fn worktrees_share_the_key_of_the_main_repo() {
    let (fake, env) = TestEnv::with_fake();
    let main = env.dir.path().join("mainrepo");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let wt = env.dir.path().join("elsewhere");
    git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    env.hook("tool", &env.input("PostToolUse", json!({"tool_name": "Bash", "cwd": wt})));
    assert_eq!(project_key_sent(&fake), "local/mainrepo");
}

#[test]
fn worktree_of_a_repo_with_origin_uses_the_remote() {
    let (fake, env) = TestEnv::with_fake();
    let main = env.dir.path().join("mainrepo");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["remote", "add", "origin", "https://github.com/alex/homelab.git"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let wt = env.dir.path().join("wt");
    git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    env.hook("tool", &env.input("PostToolUse", json!({"tool_name": "Bash", "cwd": wt})));
    assert_eq!(project_key_sent(&fake), "github.com/alex/homelab");
}

#[test]
fn non_git_directory_uses_its_basename() {
    let (fake, env) = TestEnv::with_fake();
    env.hook("tool", &env.input("PostToolUse", json!({"tool_name": "Bash"})));
    assert_eq!(project_key_sent(&fake), "local/proj");
}

#[test]
fn malformed_stdin_is_ignored() {
    let (fake, env) = TestEnv::with_fake();
    let out = crate::env::TestEnv::hook_raw(&env, "tool", "not json");
    assert_eq!(out.code, Some(0));
    assert!(fake.reqs().is_empty());
}

#[test]
fn concurrent_hooks_get_distinct_sequence_numbers() {
    let (fake, env) = TestEnv::with_fake();
    let env = std::sync::Arc::new(env);
    let threads: Vec<_> = (0..12)
        .map(|n| {
            let env = env.clone();
            std::thread::spawn(move || {
                env.hook("tool", &env.input("PostToolUse", json!({"tool_name": "Bash", "tool_input": {"n": n}})));
            })
        })
        .collect();
    threads.into_iter().for_each(|t| t.join().unwrap());
    let mut seqs: Vec<_> = fake.events().iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    seqs.sort();
    assert_eq!(seqs, (0..12).collect::<Vec<_>>());
}
