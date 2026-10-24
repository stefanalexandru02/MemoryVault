//! A throwaway machine: memory home, project directory, fake `claude`, and a way to run the binary.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::fake_server::Fake;

pub const SESSION: &str = "sess-1";

pub struct Out {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub struct TestEnv {
    pub dir: tempfile::TempDir,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub claude_state: PathBuf,
    pub extra_env: Vec<(String, String)>,
}

impl TestEnv {
    pub fn new(url: &str) -> TestEnv {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("memhome");
        let cwd = dir.path().join("work").join("proj");
        let claude_state = dir.path().join("claude-state");
        for d in [&home, &cwd, &claude_state, &dir.path().join("bin")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let env = TestEnv { dir, home, cwd, claude_state, extra_env: vec![] };
        env.write_config(url);
        env
    }

    pub fn with_fake() -> (Fake, TestEnv) {
        let fake = Fake::start();
        let env = TestEnv::new(&fake.url);
        (fake, env)
    }

    pub fn write_config(&self, url: &str) {
        std::fs::write(
            self.home.join("config"),
            format!("MEMORY_URL={url}\nMEMORY_TOKEN=mem_abcd1234_secret\nDEVICE_ID=laptop-x1\n"),
        )
        .unwrap();
    }

    /// Hook stdin: the common fields plus `extra`.
    pub fn input(&self, event: &str, extra: Value) -> Value {
        let mut v = json!({"session_id": SESSION, "cwd": self.cwd, "hook_event_name": event});
        v.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
        v
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mem-hook"));
        let path = format!("{}:{}", self.dir.path().join("bin").display(), std::env::var("PATH").unwrap_or_default());
        cmd.args(args)
            .env_remove("MEMORY_OBSERVER")
            .env_remove("MEMORY_URL")
            .env_remove("MEMORY_TOKEN")
            .env("MEMORY_HOME", &self.home)
            .env("HOME", self.dir.path())
            .env("PATH", path)
            .envs(self.extra_env.iter().map(|(k, v)| (k, v)));
        cmd
    }

    fn finish(mut cmd: Command, stdin: Option<String>) -> Out {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut pipe = child.stdin.take().unwrap();
        let _ = pipe.write_all(stdin.unwrap_or_default().as_bytes());
        drop(pipe);
        let out = child.wait_with_output().unwrap();
        Out {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Runs `mem-hook <event>` with the JSON on stdin.
    pub fn hook(&self, event: &str, input: &Value) -> Out {
        Self::finish(self.command(&[event]), Some(input.to_string()))
    }

    pub fn hook_raw(&self, event: &str, stdin: &str) -> Out {
        Self::finish(self.command(&[event]), Some(stdin.to_string()))
    }

    pub fn hook_with_env(&self, event: &str, input: &Value, key: &str, value: &str) -> Out {
        let mut cmd = self.command(&[event]);
        cmd.env(key, value);
        Self::finish(cmd, Some(input.to_string()))
    }

    /// Runs `mem-hook observe ...` to completion.
    pub fn observe(&self, args: &[&str]) -> Out {
        let mut all = vec!["observe"];
        all.extend_from_slice(args);
        Self::finish(self.command(&all), None)
    }

    pub fn spool_lines(&self) -> Vec<Value> {
        let dir = self.home.join("spool");
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect())
            .unwrap_or_default();
        files.sort();
        files
            .iter()
            .flat_map(|f| {
                std::fs::read_to_string(f)
                    .unwrap()
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect::<Vec<Value>>()
            })
            .collect()
    }

    /// Installs a fake `claude` on PATH. It records its arguments, environment and working
    /// directory per call in `claude_state`, and prints `reply.<n>` (or `reply`).
    pub fn install_claude(&self, default_reply: &str) {
        let state = self.claude_state.display();
        let script = format!(
            r#"#!/bin/bash
S='{state}'
n=$(( $(cat "$S/count" 2>/dev/null || echo 0) + 1 ))
echo $n > "$S/count"
printf '%s\0' "$@" > "$S/args.$n"
printf '%s' "$MEMORY_OBSERVER" > "$S/observer.$n"
pwd > "$S/pwd.$n"
cat > "$S/stdin.$n"
[ -f "$S/sleep.$n" ] && sleep "$(cat "$S/sleep.$n")"
[ -f "$S/stderr.$n" ] && cat "$S/stderr.$n" >&2
if [ -f "$S/reply.$n" ]; then cat "$S/reply.$n"; else cat "$S/reply"; fi
exit "$(cat "$S/exit.$n" 2>/dev/null || echo 0)"
"#
        );
        let path = self.dir.path().join("bin").join("claude");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(self.claude_state.join("reply"), default_reply).unwrap();
    }

    pub fn claude_script(&self, call: usize, kind: &str, content: &str) {
        std::fs::write(self.claude_state.join(format!("{kind}.{call}")), content).unwrap();
    }

    pub fn claude_calls(&self) -> usize {
        std::fs::read_to_string(self.claude_state.join("count")).map_or(0, |s| s.trim().parse().unwrap())
    }

    pub fn claude_args(&self, call: usize) -> Vec<String> {
        let raw = std::fs::read(self.claude_state.join(format!("args.{call}"))).unwrap();
        let mut parts: Vec<String> = raw.split(|b| *b == 0).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
        parts.pop(); // trailing NUL
        parts
    }

    pub fn claude_file(&self, call: usize, kind: &str) -> String {
        std::fs::read_to_string(self.claude_state.join(format!("{kind}.{call}"))).unwrap()
    }
}

/// What `claude -p --output-format json` prints on success.
pub fn claude_ok(result_text: &str) -> String {
    json!({
        "type": "result", "subtype": "success", "is_error": false, "result": result_text,
        "total_cost_usd": 0.012, "duration_ms": 8400, "session_id": "claude-internal",
        "modelUsage": {"claude-sonnet-4": {}},
    })
    .to_string()
}
