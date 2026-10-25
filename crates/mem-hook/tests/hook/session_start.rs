//! SessionStart: additionalContext output, resume, cached fallback.

use serde_json::{Value, json};

use crate::env::TestEnv;

const BLOCK: &str = "## Memory\n### Profile\n- Alex runs a homelab\n";

fn start(env: &TestEnv, source: &str) -> crate::env::Out {
    env.hook("session-start", &env.input("SessionStart", json!({"source": source, "model": "opus"})))
}

#[test]
fn prints_hook_specific_output_and_caches_the_block() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().context = Some(BLOCK.into());
    let out = start(&env, "startup");
    assert_eq!(out.code, Some(0));
    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap();
    assert_eq!(v, json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": BLOCK}}));

    // A system event first, then the context request with project, device and source.
    let events = fake.events();
    assert_eq!(events[0]["kind"], "system");
    assert_eq!(events[0]["payload"], json!({"source": "startup", "model": "opus"}));
    let session = &fake.matching("POST", "/v1/sessions")[0];
    assert_eq!(session.body["metadata"], json!({"source": "startup", "model": "opus"}));
    let ctx = &fake.matching("GET", "/v1/context")[0];
    assert_eq!(ctx.path, "/v1/context?project_key=local%2Fproj&device_id=laptop-x1&source=startup");
    assert_eq!(ctx.header("authorization"), Some("Bearer mem_abcd1234_secret"));
    assert_eq!(std::fs::read_to_string(env.home.join("cache/local/proj.md")).unwrap(), BLOCK);
}

#[test]
fn config_injection_is_sent_as_mode() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().context = Some(BLOCK.into());
    let config = std::fs::read_to_string(env.home.join("config")).unwrap();
    for (setting, mode) in
        [("MEM_INJECTION=lean", Some("lean")), ("MEM_INJECTION=full", Some("full")), ("MEM_INJECTION=fat", None)]
    {
        std::fs::write(env.home.join("config"), format!("{config}{setting}\n")).unwrap();
        start(&env, "startup");
        let path = fake.matching("GET", "/v1/context").last().unwrap().path.clone();
        let want = "/v1/context?project_key=local%2Fproj&device_id=laptop-x1&source=startup";
        assert_eq!(path, mode.map_or(want.to_string(), |m| format!("{want}&mode={m}")));
    }
}

#[test]
fn config_index_size_and_types_are_sent() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().context = Some(BLOCK.into());
    let want = "/v1/context?project_key=local%2Fproj&device_id=laptop-x1&source=startup";
    for (setting, suffix) in [
        ("MEM_CONTEXT_ROWS=8", "&rows=8"),
        ("MEM_CONTEXT_ROWS=0\nMEM_CONTEXT_TYPES=Decision,bugfix,opinion", "&types=decision,bugfix"),
        ("MEM_CONTEXT_ROWS=31\nMEM_CONTEXT_TYPES=everything", ""),
        ("MEM_CONTEXT_ROWS=6\nMEM_CONTEXT_TYPES=decision\nMEM_INJECTION=full", "&mode=full&rows=6&types=decision"),
    ] {
        env.write_config(&fake.url);
        env.append_config(setting);
        start(&env, "startup");
        let path = fake.matching("GET", "/v1/context").last().unwrap().path.clone();
        assert_eq!(path, format!("{want}{suffix}"), "{setting}");
    }
}

#[test]
fn resume_posts_the_event_but_returns_no_context() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().context = Some(BLOCK.into());
    let out = start(&env, "resume");
    assert_eq!((out.code, out.stdout.as_str()), (Some(0), ""));
    assert_eq!(fake.events()[0]["payload"]["source"], "resume");
    assert!(fake.matching("GET", "/v1/context").is_empty());
}

#[test]
fn clear_and_compact_fetch_context() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().context = Some(BLOCK.into());
    for source in ["clear", "compact"] {
        assert!(start(&env, source).stdout.contains("additionalContext"));
    }
    let paths: Vec<_> = fake.matching("GET", "/v1/context").iter().map(|r| r.path.clone()).collect();
    assert!(paths[0].ends_with("source=clear") && paths[1].ends_with("source=compact"), "{paths:?}");
}

#[test]
fn falls_back_to_the_cached_file_when_the_server_is_down() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().context = Some(BLOCK.into());
    start(&env, "startup");
    fake.state().down = true;
    let out = start(&env, "startup");
    assert_eq!(out.code, Some(0));
    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap();
    let text = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(text.starts_with("> memory: server unreachable, showing cached context from 20"), "{text}");
    assert!(text.ends_with(BLOCK), "{text}");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
}

#[test]
fn falls_back_when_the_connection_is_refused() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().context = Some(BLOCK.into());
    start(&env, "startup");
    // Nothing listens on this port any more.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    env.write_config(&format!("http://{}", dead.local_addr().unwrap()));
    drop(dead);
    let out = start(&env, "startup");
    assert!(out.stdout.contains("server unreachable"), "{}", out.stdout);
}

#[test]
fn no_cache_and_no_server_prints_nothing() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().down = true;
    let out = start(&env, "startup");
    assert_eq!((out.code, out.stdout.as_str()), (Some(0), ""));
}

#[test]
fn server_without_context_prints_nothing_and_does_not_use_stale_cache() {
    let (fake, env) = TestEnv::with_fake();
    fake.state().context = Some("   \n".into());
    let out = start(&env, "startup");
    assert_eq!(out.stdout, "");
    assert!(fake.matching("GET", "/v1/context").len() == 1);
}
