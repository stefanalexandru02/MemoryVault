//! `mem-hook event --client <name>`: the generic input adapter for other agent CLIs.

use serde_json::{Value, json};

use crate::env::{TestEnv, claude_ok};
use crate::fake_server::{SESSION_ID, packet};

/// A Gemini CLI hook input: the common fields plus `extra`.
fn gemini(env: &TestEnv, event: &str, extra: Value) -> Value {
    let mut v = json!({"session_id": "g-1", "transcript_path": "/tmp/t.json", "cwd": env.cwd, "hook_event_name": event,
                       "timestamp": "2026-01-01T00:00:00Z"});
    v.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
    v
}

#[test]
fn gemini_session_is_recorded_with_its_own_harness() {
    let (fake, env) = TestEnv::with_fake();
    for input in [
        gemini(&env, "BeforeAgent", json!({"prompt": "add a test"})),
        gemini(
            &env,
            "AfterTool",
            json!({"tool_name": "write_file", "tool_input": {"file_path": "a.rs"}, "tool_response": {"llmContent": "ok"}}),
        ),
        gemini(
            &env,
            "AfterAgent",
            json!({"prompt": "add a test", "prompt_response": "added", "stop_hook_active": false}),
        ),
        gemini(&env, "SessionEnd", json!({"reason": "exit"})),
    ] {
        let out = env.event("gemini", &input);
        assert_eq!((out.code, out.stdout.as_str(), out.stderr.as_str()), (Some(0), "", ""), "{input}");
    }

    let create = &fake.matching("POST", "/v1/sessions")[0];
    assert_eq!(create.path, "/v1/sessions");
    assert_eq!(create.body["harness"], "gemini");
    assert_eq!(create.body["external_session_id"], "gemini:g-1");
    assert_eq!(create.body["project_key"], "local/proj");

    let evs = fake.events();
    let kinds: Vec<_> = evs.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["user_prompt", "tool_use", "assistant_message"]);
    assert_eq!(evs[0]["payload"], json!({"prompt": "add a test"}));
    assert_eq!(evs[1]["payload"]["tool_name"], "write_file");
    assert_eq!(evs[1]["payload"]["tool_input"]["file_path"], "a.rs");
    assert_eq!(evs[1]["payload"]["tool_response"]["llmContent"], "ok");
    assert_eq!(evs[2]["payload"], json!({"last_assistant_message": "added", "stop_hook_active": false}));
    assert_eq!(evs[2]["idempotency_key"], "gemini:g-1:2");
    let ends = fake.matching("POST", &format!("/v1/sessions/{SESSION_ID}/end"));
    assert_eq!(ends[0].body["reason"], "exit");
    // The local cache is namespaced by client, so ids of different clients cannot meet.
    assert!(env.home.join("sessions/gemini_g-1").exists());
    assert!(!env.home.join("sessions/g-1").exists());
}

#[test]
fn the_documented_generic_event_is_accepted() {
    let (fake, env) = TestEnv::with_fake();
    let base = json!({"session_id": "x9", "cwd": env.cwd});
    let with = |extra: Value| {
        let mut v = base.clone();
        v.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap());
        v
    };
    for input in [
        with(json!({"event": "prompt", "text": "hello"})),
        with(json!({"event": "tool", "tool_name": "ls", "tool_input": {"path": "."}})),
        with(json!({"event": "assistant", "text": "hi there"})),
        with(json!({"event": "end"})),
    ] {
        assert_eq!(env.event("my-agent", &input).code, Some(0));
    }
    assert_eq!(fake.matching("POST", "/v1/sessions")[0].body["harness"], "my-agent");
    let kinds: Vec<_> = fake.events().iter().map(|e| e["kind"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds, ["user_prompt", "tool_use", "assistant_message"]);
    assert_eq!(fake.events()[2]["payload"]["last_assistant_message"], "hi there");
    assert_eq!(fake.matching("POST", &format!("/v1/sessions/{SESSION_ID}/end")).len(), 1);
}

#[test]
fn claude_code_as_a_client_keeps_the_plain_session_id() {
    let (fake, env) = TestEnv::with_fake();
    env.event("claude-code", &json!({"session_id": "c1", "cwd": env.cwd, "event": "prompt", "text": "hi"}));
    let create = &fake.matching("POST", "/v1/sessions")[0];
    assert_eq!(
        (create.body["harness"].as_str(), create.body["external_session_id"].as_str()),
        (Some("claude-code"), Some("c1"))
    );
}

#[test]
fn codex_session_start_prints_the_context_block() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().context = Some("## Memory\n- a fact\n".into());
    let input = json!({"session_id": "cx-1", "cwd": env.cwd, "hook_event_name": "SessionStart", "source": "startup",
                       "model": "gpt-x", "permission_mode": "default", "transcript_path": null});
    let out = env.event("codex", &input);
    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap();
    assert_eq!(
        v,
        json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": "## Memory\n- a fact\n"}})
    );
    assert_eq!(fake.matching("POST", "/v1/sessions")[0].body["harness"], "codex");
    assert_eq!(fake.events()[0]["payload"], json!({"source": "startup", "model": "gpt-x"}));
}

#[test]
fn codex_stop_with_the_hook_already_active_or_no_message_posts_nothing() {
    let (fake, env) = TestEnv::with_fake();
    let stop = |extra: Value| {
        let mut v = json!({"session_id": "cx-1", "cwd": env.cwd, "hook_event_name": "Stop"});
        v.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap());
        v
    };
    env.event("codex", &stop(json!({"last_assistant_message": "x", "stop_hook_active": true})));
    env.event("codex", &stop(json!({"last_assistant_message": null, "stop_hook_active": false})));
    assert!(fake.reqs().is_empty());
}

#[test]
fn spooled_events_replay_with_their_harness() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    env.event("codex", &json!({"session_id": "cx-2", "cwd": env.cwd, "event": "prompt", "text": "queued"}));
    assert_eq!(env.spool_lines()[0]["harness"], "codex");
    fake.state().down = false;
    // A Claude Code session start drains the spool.
    env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    let creates: Vec<_> =
        fake.matching("POST", "/v1/sessions").into_iter().filter(|r| r.path == "/v1/sessions").collect();
    assert_eq!(creates[0].body["harness"], "codex");
    assert_eq!(creates[0].body["external_session_id"], "codex:cx-2");
    assert!(env.spool_lines().is_empty());
}

#[test]
fn spool_lines_from_before_the_harness_field_replay_as_claude_code() {
    let (fake, env) = TestEnv::with_fake();
    let spool = env.home.join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    let line = json!({"claude_session_id": "old-1", "project_key": "local/proj", "cwd": env.cwd, "op": "event",
                      "occurred_at": "2026-01-01T00:00:00.000Z", "seq": 0, "kind": "user_prompt",
                      "payload": {"prompt": "old"}});
    std::fs::write(spool.join("20260101.jsonl"), format!("{line}\n")).unwrap();
    env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    let creates: Vec<_> =
        fake.matching("POST", "/v1/sessions").into_iter().filter(|r| r.path == "/v1/sessions").collect();
    assert_eq!(creates[0].body["harness"], "claude-code");
    assert_eq!(creates[0].body["external_session_id"], "old-1");
}

#[test]
fn bad_input_never_fails_the_host_and_posts_nothing() {
    let (fake, env) = TestEnv::with_fake();
    let good = json!({"session_id": "s", "cwd": env.cwd, "event": "prompt", "text": "x"});
    for (args, stdin) in [
        (vec!["event"], good.to_string()),
        (vec!["event", "--client"], good.to_string()),
        (vec!["event", "--client", "Bad Name"], good.to_string()),
        (vec!["event", "--client", "codex"], "not json".to_string()),
        (vec!["event", "--client", "codex"], "[1,2]".to_string()),
        (vec!["event", "--client", "codex"], json!({"event": "prompt", "text": "x"}).to_string()),
        (vec!["event", "--client", "codex"], json!({"session_id": "", "event": "prompt", "text": "x"}).to_string()),
        (vec!["event", "--client", "codex"], json!({"session_id": "s", "event": "dance"}).to_string()),
        (vec!["event", "--client", "codex"], json!({"session_id": "s", "event": "prompt", "text": ""}).to_string()),
    ] {
        let out = env.run(&args, &stdin);
        assert_eq!(out.code, Some(0), "{args:?}: {}", out.stderr);
    }
    assert!(fake.reqs().is_empty(), "{:#?}", fake.reqs());
}

#[test]
fn excluded_projects_record_nothing() {
    let (fake, env) = TestEnv::with_fake();
    let input = json!({"session_id": "s", "cwd": env.cwd, "event": "prompt", "text": "x"});
    std::fs::write(env.home.join("exclude"), "local/proj\n").unwrap();
    env.event("codex", &input);
    assert!(fake.reqs().is_empty());
    std::fs::remove_file(env.home.join("exclude")).unwrap();
    env.event("codex", &input);
    assert_eq!(fake.events().len(), 1);
}

#[test]
fn a_machine_without_claude_leaves_the_work_to_one_that_has_it() {
    let (fake, env) = TestEnv::with_fake();
    std::fs::write(
        env.home.join("config"),
        format!("MEMORY_URL={}\nMEMORY_TOKEN=mem_abcd1234_secret\nCLAUDE_BIN=/nonexistent/claude\n", fake.url),
    )
    .unwrap();
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    env.event("gemini", &gemini(&env, "AfterAgent", json!({"prompt_response": "done"})));
    env.event("gemini", &gemini(&env, "SessionEnd", json!({"reason": "exit"})));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(fake.matching("POST", "/v1/work/").is_empty(), "{:#?}", fake.reqs());
}

#[test]
fn a_machine_with_claude_runs_the_observer_for_other_clients_too() {
    let (fake, env) = TestEnv::with_fake();
    env.install_claude(&claude_ok(r#"{"observations":[]}"#));
    fake.queue_packet(packet("pkt-1", "extract", "sonnet"));
    env.event("gemini", &gemini(&env, "AfterAgent", json!({"prompt_response": "done"})));
    fake.wait_for("result post", |r| r.iter().any(|q| q.path == "/v1/work/pkt-1/result"));
}
