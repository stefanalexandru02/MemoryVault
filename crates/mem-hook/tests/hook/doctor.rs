//! `mem-hook doctor`: one line per check, exit 1 when one fails.

use serde_json::{Value, json};

use crate::env::{Out, TestEnv};

const ENABLED: &str = r#"{"enabledPlugins": {"memory@memoryvault": true}}"#;

fn healthy() -> (crate::fake_server::Fake, TestEnv) {
    let (fake, env) = TestEnv::with_fake();
    env.write_claude_settings(&serde_json::from_str(ENABLED).unwrap());
    (fake, env)
}

fn deliver_one(env: &TestEnv) {
    let input =
        env.input("PostToolUse", json!({"tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_response": {}}));
    assert_eq!(env.hook("tool", &input).code, Some(0));
}

fn checks(out: &Out) -> Vec<Value> {
    let v: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {}", out.stdout));
    v["checks"].as_array().unwrap().clone()
}

fn check<'a>(checks: &'a [Value], name: &str) -> &'a Value {
    checks.iter().find(|c| c["name"] == name).unwrap_or_else(|| panic!("no check {name} in {checks:?}"))
}

#[test]
fn everything_in_order_exits_zero() {
    let (_fake, env) = healthy();
    deliver_one(&env);
    let out = env.doctor(&[]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    for name in
        ["config", "server", "token", "environment", "hook binary", "plugin", "spool", "session cache", "last delivery"]
    {
        assert!(out.stdout.lines().any(|l| l.starts_with("[ ok ]") && l.contains(name)), "{name}: {}", out.stdout);
    }
    assert!(!out.stdout.contains("fix:"));
}

#[test]
fn json_has_the_same_checks_without_marks() {
    let (_fake, env) = healthy();
    deliver_one(&env);
    let out = env.doctor(&["--json"]);
    assert_eq!(out.code, Some(0));
    let all = checks(&out);
    assert_eq!(all.len(), 9);
    assert!(all.iter().all(|c| c["status"] == "ok" && c["fix"].is_null()), "{all:?}");
    assert!(check(&all, "token")["detail"].as_str().unwrap().contains("events:write"));
}

#[test]
fn missing_config_fails_with_the_fix() {
    let (_fake, env) = healthy();
    std::fs::remove_file(env.home.join("config")).unwrap();
    let out = env.doctor(&[]);
    assert_eq!(out.code, Some(1));
    assert!(out.stdout.contains("[FAIL] config"), "{}", out.stdout);
    assert!(out.stdout.contains("fix: make connect-local"), "{}", out.stdout);
    assert!(out.stdout.contains("[warn] server"), "the network checks are skipped, not failed: {}", out.stdout);
}

#[test]
fn config_with_a_broken_line_or_a_missing_key_fails() {
    let (_fake, env) = healthy();
    std::fs::write(env.home.join("config"), "MEMORY_URL=http://x\nMEMORY_TOKEN mem_abc\n").unwrap();
    let all = checks(&env.doctor(&["--json"]));
    let config = check(&all, "config");
    assert_eq!(config["status"], "fail");
    assert!(config["detail"].as_str().unwrap().contains("line 2"));

    std::fs::write(env.home.join("config"), "MEMORY_URL=http://x\n").unwrap();
    let all = checks(&env.doctor(&["--json"]));
    assert!(check(&all, "config")["detail"].as_str().unwrap().contains("missing MEMORY_TOKEN"));
}

#[test]
fn unreachable_server_fails() {
    let (_fake, env) = healthy();
    env.write_config("http://127.0.0.1:1");
    let out = env.doctor(&["--json"]);
    assert_eq!(out.code, Some(1));
    let all = checks(&out);
    let server = check(&all, "server");
    assert_eq!(server["status"], "fail");
    assert!(server["fix"].as_str().unwrap().contains("curl http://127.0.0.1:1/health"));
    assert_eq!(check(&all, "token")["status"], "warn");
}

#[test]
fn rejected_token_fails() {
    let (fake, env) = healthy();
    fake.state().me_status = 401;
    let out = env.doctor(&[]);
    assert_eq!(out.code, Some(1));
    assert!(out.stdout.contains("[FAIL] token") && out.stdout.contains("HTTP 401 invalid_token"), "{}", out.stdout);
    assert!(out.stdout.contains("mint a new token"), "{}", out.stdout);
}

#[test]
fn server_down_is_reported() {
    let (fake, env) = healthy();
    fake.state().down = true;
    let out = env.doctor(&[]);
    assert_eq!(out.code, Some(1));
    assert!(out.stdout.contains("[FAIL] server") && out.stdout.contains("HTTP 503"), "{}", out.stdout);
}

#[test]
fn spool_depth_and_age_decide_between_warn_and_fail() {
    let (fake, env) = healthy();
    fake.state().down = true;
    deliver_one(&env);
    deliver_one(&env);
    fake.state().down = false;
    let all = checks(&env.doctor(&["--json"]));
    let spool = check(&all, "spool");
    assert_eq!(spool["status"], "warn", "a fresh backlog is only a warning");
    assert!(spool["detail"].as_str().unwrap().starts_with("2 undelivered event(s)"), "{spool}");

    let file = std::fs::read_dir(env.home.join("spool"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .unwrap();
    let aged: String = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|l| {
            let mut entry: Value = serde_json::from_str(l).unwrap();
            entry["occurred_at"] = json!("2001-01-01T00:00:00.000Z");
            format!("{entry}\n")
        })
        .collect();
    std::fs::write(&file, aged).unwrap();
    let out = env.doctor(&["--json"]);
    assert_eq!(out.code, Some(1));
    let all = checks(&out);
    let spool = check(&all, "spool");
    assert_eq!(spool["status"], "fail");
    assert!(spool["fix"].as_str().unwrap().contains("start a Claude Code session"));
}

#[test]
fn plugin_states() {
    let (_fake, env) = healthy();
    env.write_claude_settings(&json!({"enabledPlugins": {"memory@memoryvault": false}}));
    let all = checks(&env.doctor(&["--json"]));
    let plugin = check(&all, "plugin");
    assert_eq!(plugin["status"], "fail");
    assert_eq!(plugin["fix"], "claude plugin enable memory@memoryvault");

    env.write_claude_settings(&json!({"enabledPlugins": {"other@x": true}}));
    let all = checks(&env.doctor(&["--json"]));
    assert_eq!(check(&all, "plugin")["status"], "fail");

    env.write_claude_settings(&json!({"enabledPlugins": {"memory@memoryvault": true}, "disableAllHooks": true}));
    let all = checks(&env.doctor(&["--json"]));
    assert!(check(&all, "plugin")["detail"].as_str().unwrap().contains("disableAllHooks"));

    std::fs::remove_file(env.dir.path().join(".claude/settings.json")).unwrap();
    let out = env.doctor(&["--json"]);
    assert_eq!(check(&checks(&out), "plugin")["status"], "warn", "no settings file: cannot tell");
}

#[test]
fn plugin_enabled_for_the_project_counts() {
    let (_fake, env) = healthy();
    env.write_claude_settings(&json!({}));
    let project = env.cwd.join(".claude");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("settings.local.json"), ENABLED).unwrap();
    let all = checks(&env.doctor(&["--json"]));
    assert_eq!(check(&all, "plugin")["status"], "ok");
}

#[test]
fn observer_variable_disables_capture_and_is_reported() {
    let (_fake, mut env) = healthy();
    env.extra_env.push(("MEMORY_OBSERVER".into(), "1".into()));
    let out = env.doctor(&[]);
    assert_eq!(out.code, Some(1));
    assert!(
        out.stdout.contains("[FAIL] environment") && out.stdout.contains("unset MEMORY_OBSERVER"),
        "{}",
        out.stdout
    );
}

#[test]
fn last_delivery_is_recorded_by_the_first_accepted_request() {
    let (_fake, env) = healthy();
    let before = checks(&env.doctor(&["--json"]));
    assert_eq!(check(&before, "last delivery")["status"], "warn");
    assert!(!env.home.join("last-delivery").exists());
    deliver_one(&env);
    assert!(env.home.join("last-delivery").exists());
    let after = checks(&env.doctor(&["--json"]));
    assert_eq!(check(&after, "last delivery")["status"], "ok");
}

#[test]
fn server_flag_appends_the_server_checks_or_explains_why_not() {
    let (fake, env) = healthy();
    deliver_one(&env);
    let skipped = env.doctor(&["--server"]);
    assert_eq!(skipped.code, Some(0), "{}", skipped.stdout);
    assert!(
        skipped.stdout.contains("[warn] server checks") && skipped.stdout.contains("MEMORY_ADMIN_TOKEN"),
        "{}",
        skipped.stdout
    );

    fake.state().admin_doctor = (
        200,
        json!({"checks": [
            {"name": "database", "status": "ok", "detail": "19 migrations", "fix": null},
            {"name": "embed queue", "status": "fail", "detail": "stuck", "fix": "memvault worker"},
        ]}),
    );
    let out = env.doctor(&["--server", "--json"]);
    assert_eq!(out.code, Some(1), "a failing server check fails the run");
    let all = checks(&out);
    assert_eq!(check(&all, "server: database")["status"], "ok");
    assert_eq!(check(&all, "server: embed queue")["fix"], "memvault worker");
    let req = fake.matching("GET", "/v1/admin/doctor");
    assert_eq!(req[0].header("authorization"), Some("Bearer mem_abcd1234_secret"));

    std::fs::write(
        env.home.join("config"),
        format!("{}MEMORY_ADMIN_TOKEN=mem_admin_token\n", std::fs::read_to_string(env.home.join("config")).unwrap()),
    )
    .unwrap();
    env.doctor(&["--server"]);
    assert_eq!(
        fake.matching("GET", "/v1/admin/doctor").last().unwrap().header("authorization"),
        Some("Bearer mem_admin_token")
    );
}

#[test]
fn unknown_flags_are_a_usage_error() {
    let (_fake, env) = healthy();
    let out = env.doctor(&["--frobnicate"]);
    assert_eq!(out.code, Some(2));
    assert!(out.stderr.contains("usage: mem-hook doctor"));
}
