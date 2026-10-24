//! Spooling on failure and ordered replay at session start.

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
    assert_eq!(ends[0].body, json!({"reason": "logout"}));
}
