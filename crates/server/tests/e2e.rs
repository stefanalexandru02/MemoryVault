//! End to end: the real `memvault` binary migrates a fresh database and mints tokens, the server
//! runs on a random port (FakeTei), and the real `mem-hook` binary plays one Claude Code turn
//! (session-start, prompt, 8 tool calls, stop) with a fake `claude` on PATH as the observer model.
//! Then: observations and a prompt summary land, one worker pass embeds them, the SessionStart block
//! lists them, MCP `memory_search` finds them, and a second device sees the turn in
//! since-you-were-here.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use memvault_core::test_support::{TEST_PEPPER, repo_dir, test_settings, test_state_with};
use memvault_server::{all_appliers, serve};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

const PROJECT: &str = "github.com/alex/e2e-demo";
const TITLE: &str = "Config parser accepts trailing commas in arrays";
const NEXT_STEPS: &str = "Add a regression test for nested arrays";

/// `DATABASE_URL` pointed at this test's database. `PgConnectOptions` does not expose the password,
/// so the URL is derived from the environment rather than rebuilt from the pool's options.
fn database_url(pool: &PgPool) -> String {
    let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").expect("DATABASE_URL")).unwrap();
    url.set_path(pool.connect_options().get_database().unwrap());
    url.to_string()
}

/// `memvault <args>` against `db`; stdout parsed as JSON (null when empty).
fn memvault(db: &str, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_memvault"))
        .args(args)
        .env("DATABASE_URL", db)
        .env_remove("DATABASE_SYSTEM_URL")
        .env("TOKEN_PEPPER", TEST_PEPPER)
        .env("FAKE_TEI", "1")
        .env("PROMPTS_DIR", repo_dir().join("prompts"))
        .env("SCHEMAS_DIR", repo_dir().join("schemas"))
        .env("LOG_FORMAT", "text")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "memvault {args:?}: {}\n{stdout}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_str(&stdout).unwrap_or(Value::Null)
}

/// Builds `mem-hook` (another package, so no `CARGO_BIN_EXE_` for it) into this target directory.
fn mem_hook_binary() -> PathBuf {
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "-p", "mem-hook"])
        .current_dir(repo_dir())
        .status()
        .unwrap();
    assert!(status.success(), "cargo build -p mem-hook failed");
    // target/<profile>/deps/e2e-<hash> -> target/<profile>/mem-hook
    let exe = std::env::current_exe().unwrap();
    exe.parent().and_then(Path::parent).unwrap().join("mem-hook")
}

/// One machine: memory home, a git project, a fake `claude` on PATH.
struct Machine {
    dir: tempfile::TempDir,
    hook: PathBuf,
}

impl Machine {
    fn new(hook: PathBuf, url: &str, token: &str, device: &str) -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in ["memhome", "bin", "claude", "work/e2e-demo"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(
            root.join("memhome/config"),
            format!("MEMORY_URL={url}\nMEMORY_TOKEN={token}\nDEVICE_ID={device}\n"),
        )
        .unwrap();
        let project = root.join("work/e2e-demo");
        for args in [&["init", "-q"][..], &["remote", "add", "origin", "https://github.com/alex/e2e-demo.git"]] {
            let ok = Command::new("git").args(args).current_dir(&project).status().unwrap().success();
            assert!(ok, "git {args:?}");
        }
        let observer = json!({
            "skip": false,
            "observations": [{
                "type": "bugfix", "title": TITLE,
                "subtitle": "parse_array skipped the last element after a trailing comma",
                "narrative": "The config parser treated a trailing comma as the end of input. parse_array now peeks for the closing bracket.",
                "facts": ["parse_array accepts a trailing comma", "src/parser.rs holds the array grammar"],
                "concepts": ["problem-solution"], "entities": ["config-parser"], "files": ["src/parser.rs"],
                "importance": 0.8, "sensitivity": "normal", "source_event_seqs": [3, 4]
            }],
            "fact_candidates": []
        });
        let summary = json!({
            "request": "Fix the config parser dropping array elements after a trailing comma",
            "investigated": "src/parser.rs array grammar", "learned": "The tokenizer emits no token for a trailing comma",
            "completed": "parse_array accepts trailing commas; tests pass", "next_steps": NEXT_STEPS, "notes": null
        });
        for (name, text) in [("observer", observer), ("summary", summary)] {
            let reply = json!({"type": "result", "subtype": "success", "is_error": false, "result": text.to_string(),
                               "total_cost_usd": 0.01, "duration_ms": 1200, "session_id": "observer-run"});
            std::fs::write(root.join(format!("claude/{name}.json")), reply.to_string()).unwrap();
        }
        // The summary prompts say "progress summaries" / "end-of-session summary"; the observer's does not.
        let script = format!(
            "#!/bin/sh\nS='{}'\necho call >> \"$S/calls\"\nif printf '%s\\n' \"$@\" | grep -q 'progress summaries\\|end-of-session summary'; then\n  cat \"$S/summary.json\"\nelse\n  cat \"$S/observer.json\"\nfi\n",
            root.join("claude").display()
        );
        let claude = root.join("bin/claude");
        std::fs::write(&claude, script).unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        Machine { dir, hook }
    }

    /// Runs `mem-hook <event>` with Claude Code's hook input on stdin; returns stdout. Async: the
    /// server runs on this test's (single-threaded) runtime.
    async fn hook(&self, event: &str, hook_event: &str, extra: Value) -> String {
        use tokio::io::AsyncWriteExt;
        let root = self.dir.path();
        let mut input =
            json!({"session_id": "claude-session-1", "cwd": root.join("work/e2e-demo"), "hook_event_name": hook_event});
        input.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let path = format!("{}:{}", root.join("bin").display(), std::env::var("PATH").unwrap_or_default());
        let mut child = tokio::process::Command::new(&self.hook)
            .arg(event)
            .env_remove("MEMORY_OBSERVER")
            .env_remove("MEMORY_URL")
            .env_remove("MEMORY_TOKEN")
            .env("MEMORY_HOME", root.join("memhome"))
            .env("HOME", root)
            .env("PATH", path)
            .env("MEMORY_DEBUG", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.to_string().as_bytes()).await.unwrap();
        drop(stdin);
        let out = child.wait_with_output().await.unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "mem-hook {event}: {stderr}");
        assert!(stderr.is_empty(), "mem-hook {event} complained: {stderr}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn observer_log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("memhome/observe.log")).unwrap_or_default()
    }
}

/// Polls `check` until it returns `Some`, failing after `secs` with `what`.
async fn eventually<T, F: AsyncFn() -> Option<T>>(secs: u64, what: impl Fn() -> String, check: F) -> T {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = check().await {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {}", what());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn context(http: &reqwest::Client, base: &str, token: &str, device: &str) -> String {
    let resp = http
        .get(format!("{base}/v1/context?project_key={PROJECT}&device_id={device}&source=startup"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.text().await.unwrap()
}

#[sqlx::test(migrations = false)]
async fn hook_to_recall_end_to_end(pool: PgPool) {
    let db = database_url(&pool);

    // Fresh database: migrate, then an admin creates the user (with a private space) and two device tokens.
    memvault(&db, &["migrate", "--dsn", &db]);
    let user = memvault(&db, &["admin", "user", "create", "alex", "--display-name", "Alex"]);
    let space_id = user["default_space_id"].as_str().expect("private space").to_string();
    let laptop =
        memvault(&db, &["admin", "token", "create", "--user", "alex", "--client", "laptop-x1", "--kind", "device"]);
    let desktop =
        memvault(&db, &["admin", "token", "create", "--user", "alex", "--client", "desktop-y2", "--kind", "device"]);
    let (laptop, desktop) = (laptop["token"].as_str().unwrap(), desktop["token"].as_str().unwrap());

    let hook = mem_hook_binary();

    // The server on a random port, FakeTei, every applier.
    let (state, _tei) = test_state_with(pool.clone(), test_settings(), all_appliers());
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", tcp.local_addr().unwrap());
    let stop = CancellationToken::new();
    let server = tokio::spawn(serve(state.clone(), tcp, stop.clone().cancelled_owned()));

    // One Claude Code turn on the laptop through the real hook binary.
    let machine = Machine::new(hook, &base, laptop, "laptop-x1");
    machine.hook("session-start", "SessionStart", json!({"source": "startup", "model": "claude-opus"})).await;
    machine
        .hook("prompt", "UserPromptSubmit", json!({"prompt": "the config parser drops the last array element"}))
        .await;
    for i in 0..8 {
        let (tool, input) = match i % 3 {
            0 => ("Read", json!({"file_path": "src/parser.rs"})),
            1 => ("Edit", json!({"file_path": "src/parser.rs", "old_string": "a", "new_string": "b"})),
            _ => ("Bash", json!({"command": "cargo test parser"})),
        };
        let extra = json!({"tool_name": tool, "tool_input": input, "tool_response": {"ok": true, "i": i},
                           "tool_use_id": format!("toolu_{i}"), "duration_ms": 40});
        machine.hook("tool", "PostToolUse", extra).await;
    }
    let stop_input = json!({"last_assistant_message": "Fixed: trailing commas parse now.", "stop_hook_active": false});
    machine.hook("stop", "Stop", stop_input).await;

    let session: (uuid::Uuid, String) =
        sqlx::query_as("SELECT id, project_key FROM sessions").fetch_one(&pool).await.unwrap();
    assert_eq!(session.1, PROJECT);
    assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 11, "system + prompt + 8 tools + stop");

    // The detached observers lease the packets, run the fake claude and post results.
    let log = || format!("observations and a prompt summary; observer log:\n{}", machine.observer_log());
    eventually(60, log, async || {
        let obs = count(&pool, "SELECT count(*) FROM observations").await;
        let sums = count(&pool, "SELECT count(*) FROM summaries WHERE grain = 'prompt'").await;
        (obs >= 1 && sums >= 1).then_some(())
    })
    .await;
    let title: String = sqlx::query_scalar("SELECT title FROM observations").fetch_one(&pool).await.unwrap();
    assert_eq!(title, TITLE);
    let summary_next: String =
        sqlx::query_scalar("SELECT next_steps FROM summaries WHERE grain = 'prompt'").fetch_one(&pool).await.unwrap();
    assert_eq!(summary_next, NEXT_STEPS);
    let space: uuid::Uuid = sqlx::query_scalar("SELECT space_id FROM observations").fetch_one(&pool).await.unwrap();
    assert_eq!(space.to_string(), space_id);

    // One worker pass embeds every queued row.
    let claimed = memvault_worker::embed_pass(&state, "e2e-worker").await.unwrap();
    assert!(claimed >= 2, "observation and summary embed jobs, got {claimed}");
    assert!(count(&pool, "SELECT count(*) FROM embeddings WHERE owner_table = 'observations'").await >= 1);
    assert!(count(&pool, "SELECT count(*) FROM embeddings WHERE owner_table = 'summaries'").await >= 1);
    assert_eq!(count(&pool, "SELECT count(*) FROM jobs WHERE type = 'embed' AND status <> 'done'").await, 0);

    // The SessionStart block lists the observation (the cache was invalidated by the result).
    let http = reqwest::Client::new();
    let block = eventually(
        10,
        || "the observation in the context index".into(),
        async || {
            let block = context(&http, &base, laptop, "laptop-x1").await;
            block.contains(TITLE).then_some(block)
        },
    )
    .await;
    assert!(block.starts_with("## Memory"), "{block}");

    // MCP over HTTP: tools/call memory_search.
    let rpc = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": {"name": "memory_search", "arguments": {"query": "trailing comma parser"}}});
    let resp = http
        .post(format!("{base}/mcp"))
        .bearer_auth(laptop)
        .header("accept", "application/json, text/event-stream")
        .json(&rpc)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["result"]["isError"].as_bool(), Some(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains(TITLE), "{text}");

    // The second device sees the laptop's turn in since-you-were-here.
    let block = context(&http, &base, desktop, "desktop-y2").await;
    let since = block
        .split("### ")
        .find(|s| s.starts_with("Since you were here (desktop-y2"))
        .unwrap_or_else(|| panic!("no since-you-were-here section:\n{block}"));
    assert!(since.contains("laptop-x1") && since.contains(NEXT_STEPS), "{since}");

    stop.cancel();
    server.await.unwrap().unwrap();
}
