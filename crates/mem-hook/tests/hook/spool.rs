//! Spooling on failure and ordered replay at session start.

use std::time::Duration;

use serde_json::json;

use crate::env::{SESSION, TestEnv};
use crate::fake_server::SESSION_ID;

fn tool(env: &TestEnv, n: usize) {
    let out = env.hook(
        "tool",
        &env.input(
            "PostToolUse",
            json!({"tool_name": "Bash", "tool_input": {"command": format!("cmd {n}")}, "tool_response": {}}),
        ),
    );
    assert_eq!(out.code, Some(0));
}

#[test]
fn failed_posts_are_spooled_in_order_and_replayed_at_session_start() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    for n in 0..3 {
        tool(&env, n);
    }
    let spooled = env.spool_lines();
    assert_eq!(spooled.len(), 3);
    for (i, line) in spooled.iter().enumerate() {
        assert_eq!(
            (line["op"].as_str(), line["kind"].as_str(), line["seq"].as_i64()),
            (Some("event"), Some("tool_use"), Some(i as i64))
        );
        assert_eq!(line["claude_session_id"], SESSION);
        assert_eq!(line["project_key"], "local/proj");
        assert_eq!(line["payload"]["tool_input"]["command"], format!("cmd {i}"));
    }
    assert!(fake.events().is_empty());
    let mode = std::fs::metadata(env.home.join("spool")).unwrap();
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777, 0o700);

    // Server back: the next session start replays the spool oldest first, then posts its own event.
    fake.state().down = false;
    fake.state().reqs.clear();
    env.hook("session-start", &env.input("SessionStart", json!({"source": "startup"})));
    let evs = fake.events();
    let order: Vec<_> =
        evs.iter().map(|e| (e["seq"].as_i64().unwrap(), e["kind"].as_str().unwrap().to_string())).collect();
    assert_eq!(order, [(0, "tool_use".into()), (1, "tool_use".into()), (2, "tool_use".into()), (3, "system".into())]);
    assert_eq!(evs[1]["payload"]["tool_input"]["command"], "cmd 1");
    assert_eq!(evs[0]["idempotency_key"], "sess-1:0");
    assert!(env.spool_lines().is_empty());
    assert!(
        !env.home
            .join("spool")
            .read_dir()
            .unwrap()
            .flatten()
            .any(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
    );
    // The session was created once, with the project the events were recorded under.
    let created = fake.matching("POST", "/v1/sessions");
    assert_eq!(created.iter().filter(|r| r.path == "/v1/sessions").count(), 1);
}

#[test]
fn replay_stops_at_the_first_failure_and_keeps_the_rest_in_order() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    for n in 0..3 {
        tool(&env, n);
    }
    // The server takes one event and then fails: the rest of the spool stays, the new event queues behind it.
    {
        let mut s = fake.state();
        s.down = false;
        s.events_ok = Some(1);
        s.reqs.clear();
    }
    env.hook("session-start", &env.input("SessionStart", json!({"source": "startup"})));
    // Attempts: seq 0 accepted, seq 1 refused (replay stops, seq 2 is never tried), then the new event.
    let attempted: Vec<_> = fake.events().iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert_eq!(attempted, [0, 1, 3]);
    let left: Vec<_> = env.spool_lines().iter().map(|l| l["seq"].as_i64().unwrap()).collect();
    assert_eq!(left, [1, 2, 3]);

    // Next time everything goes through, oldest first.
    {
        let mut s = fake.state();
        s.events_ok = None;
        s.reqs.clear();
    }
    env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    let seqs: Vec<_> = fake.events().iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert_eq!(seqs, [1, 2, 3, 4]);
    assert!(env.spool_lines().is_empty());
}

#[test]
fn connection_refused_spools_too() {
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let env = TestEnv::new(&format!("http://{}", dead.local_addr().unwrap()));
    drop(dead);
    tool(&env, 0);
    env.hook("prompt", &env.input("UserPromptSubmit", json!({"prompt": "hello"})));
    let spooled = env.spool_lines();
    assert_eq!(spooled.iter().map(|l| l["kind"].as_str().unwrap()).collect::<Vec<_>>(), ["tool_use", "user_prompt"]);
    assert_eq!(spooled[1]["seq"], 1);
}

#[test]
fn permanently_rejected_events_are_not_spooled() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().events_status = 422;
    tool(&env, 0);
    assert!(env.spool_lines().is_empty());
}

#[test]
fn replay_drops_permanently_rejected_lines_and_continues() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    tool(&env, 0);
    fake.state().down = false;
    fake.state().events_status = 422;
    env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    assert!(env.spool_lines().is_empty());
}

#[test]
fn session_end_spooled_when_the_server_is_down_replays_later() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    env.hook("session-end", &env.input("SessionEnd", json!({"reason": "logout"})));
    assert_eq!(env.spool_lines()[0]["reason"], "logout");
    fake.state().down = false;
    env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    let ends = fake.matching("POST", &format!("/v1/sessions/{SESSION_ID}/end"));
    assert_eq!(ends[0].body["reason"], "logout");
}

fn seqs(fake: &crate::fake_server::Fake) -> Vec<i64> {
    fake.events().iter().map(|e| e["seq"].as_i64().unwrap()).collect()
}

#[test]
fn a_request_the_server_chokes_on_does_not_block_the_spool_and_is_eventually_dropped() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    for n in 0..3 {
        tool(&env, n);
    }
    {
        let mut s = fake.state();
        s.down = false;
        s.poison = Some("cmd 1".into());
        s.reqs.clear();
    }
    // The lines around the poison line are delivered, in order; it stays and counts a replay.
    env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    assert_eq!(seqs(&fake), [0, 1, 2, 3], "seq 1 was attempted");
    let left = env.spool_lines();
    assert_eq!(left.len(), 1);
    assert_eq!((left[0]["seq"].as_i64(), left[0]["replays"].as_u64()), (Some(1), Some(1)));

    for round in 2..5 {
        env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
        assert_eq!(env.spool_lines()[0]["replays"], round, "round {round}");
    }
    env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    assert!(env.spool_lines().is_empty(), "dropped after five failed replays");
}

#[test]
fn a_server_that_fails_everything_does_not_use_up_the_replays() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    for n in 0..6 {
        tool(&env, n);
    }
    {
        let mut s = fake.state();
        s.down = false;
        s.events_status = 500;
    }
    for _ in 0..6 {
        env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    }
    let left = env.spool_lines();
    assert_eq!(left.len(), 12, "6 spooled + one new event per session start");
    assert!(left.iter().all(|l| l.get("replays").is_none()), "{left:?}");
}

#[test]
fn nul_characters_never_leave_the_machine() {
    let (fake, env) = TestEnv::with_fake();
    env.hook(
        "tool",
        &env.input(
            "PostToolUse",
            json!({"tool_name": "Bash", "tool_input": {"command": "cat blob"}, "tool_response": {"stdout": "a\u{0}b"}}),
        ),
    );
    let events = fake.events();
    assert_eq!(events[0]["payload"]["tool_response"]["stdout"], "ab");
    assert!(!fake.reqs().iter().any(|r| r.body.to_string().contains("\\u0000")));
}

#[test]
fn old_spooled_lines_with_nul_are_cleaned_on_replay() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    tool(&env, 0);
    let file = std::fs::read_dir(env.home.join("spool"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .unwrap();
    let text = std::fs::read_to_string(&file).unwrap().replace("cmd 0", "cmd\\u0000 0");
    std::fs::write(&file, text).unwrap();
    {
        let mut s = fake.state();
        s.down = false;
        s.reqs.clear();
    }
    env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    assert_eq!(fake.events()[0]["payload"]["tool_input"]["command"], "cmd 0");
    assert!(env.spool_lines().is_empty());
}

#[test]
fn a_dead_writers_partial_line_does_not_block_later_files() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    tool(&env, 0);
    fake.state().down = false;
    // yesterday's file ends in a line nobody will ever finish
    let spool = env.home.join("spool");
    let stale = spool.join("20200101.jsonl");
    std::fs::write(&stale, r#"{"claude_session_id":"sess-1","op":"ev"#).unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options().write(true).open(&stale).unwrap().set_modified(old).unwrap();
    env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    assert!(!stale.exists(), "the unterminated line is discarded");
    assert!(env.spool_lines().is_empty());
    assert_eq!(seqs(&fake), [0, 1]);
}

#[test]
fn an_append_after_a_partial_line_starts_on_a_new_line() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    tool(&env, 0);
    let file = std::fs::read_dir(env.home.join("spool"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .unwrap();
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.pop(); // the writer died before the newline
    std::fs::write(&file, text).unwrap();
    tool(&env, 1);
    let content = std::fs::read_to_string(&file).unwrap();
    let valid = content.lines().filter(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()).count();
    assert_eq!(content.lines().count(), 2);
    assert_eq!(valid, 2, "{content}");
}

#[cfg(unix)]
#[test]
fn files_under_the_memory_home_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let (fake, env) = TestEnv::with_fake();
    // a home the user created by hand, with lax modes
    std::fs::set_permissions(&env.home, std::fs::Permissions::from_mode(0o755)).unwrap();
    fake.state().down = true;
    for n in 0..2 {
        tool(&env, n);
    }
    fake.state().down = false;
    fake.state().events_ok = Some(1);
    fake.state().context = Some("ctx".into());
    env.hook("session-start", &env.input("SessionStart", json!({"source": "startup"})));
    assert_eq!(mode(&env.home), 0o700);
    // a partly replayed spool file is rewritten, and must stay owner-only
    for entry in std::fs::read_dir(env.home.join("spool")).unwrap().flatten() {
        assert_eq!(mode(&entry.path()), 0o600, "{:?}", entry.path());
    }
    for dir in ["sessions", "cache/local"] {
        for entry in std::fs::read_dir(env.home.join(dir)).unwrap().flatten() {
            assert_eq!(mode(&entry.path()), 0o600, "{:?}", entry.path());
        }
    }
}

#[test]
fn a_lost_session_cache_continues_the_sequence_from_the_server() {
    let (fake, env) = TestEnv::with_fake();
    tool(&env, 0);
    tool(&env, 1);
    assert_eq!(seqs(&fake), [0, 1]);
    // the cache was pruned (30 days) and the same Claude session id comes back with --resume
    std::fs::remove_dir_all(env.home.join("sessions")).unwrap();
    fake.state().next_seq = 2;
    tool(&env, 2);
    tool(&env, 3);
    assert_eq!(seqs(&fake), [0, 1, 2, 3]);
}

#[test]
fn a_late_session_end_carries_its_own_time() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    env.hook("session-end", &env.input("SessionEnd", json!({"reason": "logout"})));
    let spooled = env.spool_lines();
    fake.state().down = false;
    env.hook("session-start", &env.input("SessionStart", json!({"source": "resume"})));
    let ends = fake.matching("POST", &format!("/v1/sessions/{SESSION_ID}/end"));
    assert_eq!(ends[0].body, json!({"reason": "logout", "occurred_at": spooled[0]["occurred_at"]}));
}
