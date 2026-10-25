//! Runs a leased packet through `claude -p` for `evals replay --prompt` (the same invocation
//! `mem-hook observe` uses, spec 8.5): no tools, one turn, hooks off, a fresh scratch directory.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(300);

/// The model's answer text for one packet (`system` prompt, `user` message).
pub async fn run(claude_bin: &str, model: &str, system: &str, user: &str) -> Result<String> {
    let dir = tempfile::Builder::new().prefix("mem-eval-").tempdir().context("scratch directory")?;
    let mut child = Command::new(claude_bin)
        .args(["-p", "--model", model, "--output-format", "json", "--disallowedTools", "*"])
        .args(["--no-session-persistence", "--max-turns", "1", "--settings", r#"{"disableAllHooks": true}"#])
        .args(["--system-prompt", system])
        .current_dir(dir.path())
        .env("MEMORY_OBSERVER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {claude_bin}"))?;
    // The prompt goes over stdin: a window of events can exceed the argv limit.
    let mut stdin = child.stdin.take().context("claude stdin")?;
    let text = user.to_string();
    let feeder = tokio::spawn(async move {
        let _ = stdin.write_all(text.as_bytes()).await;
    });
    let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("claude -p exceeded {}s", TIMEOUT.as_secs()))??;
    let _ = feeder.await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let reply = parse_reply(&stdout);
    if !out.status.success() || reply.as_ref().is_some_and(|r| r.1) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("claude -p failed ({}): {} {}", out.status, stderr.trim(), reply.map(|r| r.0).unwrap_or_default());
    }
    reply
        .map(|r| r.0)
        .with_context(|| format!("unparseable claude output: {}", stdout.chars().take(300).collect::<String>()))
}

/// `(result text, is_error)` from `claude -p --output-format json` (one object, or an array of events).
fn parse_reply(stdout: &str) -> Option<(String, bool)> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let obj = match &v {
        Value::Array(items) => items.iter().rev().find(|i| i["type"] == "result")?,
        other => other,
    };
    Some((obj["result"].as_str()?.to_string(), obj["is_error"].as_bool().unwrap_or(false)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_output_shapes() {
        assert_eq!(parse_reply(r#"{"type":"result","result":"{}","is_error":false}"#), Some(("{}".into(), false)));
        assert_eq!(
            parse_reply(r#"[{"type":"system"},{"type":"result","result":"x","is_error":true}]"#),
            Some(("x".into(), true))
        );
        assert_eq!(parse_reply("nope"), None);
    }
}
