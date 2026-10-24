//! The recorded session format: `evals/sessions/<id>.jsonl`, one JSON object per line.
//!
//! - `session`: the first line (session metadata);
//! - `event`: an event as ingested (`seq`, `kind`, `occurred_at`, clamped `payload`);
//! - `output`: the output a runner posted for a packet, keyed by window or prompt number, so a
//!   replay can apply the recorded observer instead of calling a model.

use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Line {
    Session {
        external_session_id: String,
        #[serde(default)]
        project_key: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default = "default_harness")]
        harness: String,
        #[serde(default)]
        device_id: Option<String>,
    },
    Event {
        seq: i32,
        kind: String,
        occurred_at: DateTime<Utc>,
        #[serde(default)]
        payload: Map<String, Value>,
    },
    Output {
        /// `extract`, `summarize_prompt` or `summarize_session`.
        packet: String,
        #[serde(default)]
        window_no: Option<i64>,
        #[serde(default)]
        prompt_no: Option<i64>,
        output: Value,
    },
}

fn default_harness() -> String {
    "claude-code".into()
}

/// A parsed session file.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedSession {
    pub external_session_id: String,
    pub project_key: Option<String>,
    pub cwd: Option<String>,
    pub harness: String,
    pub device_id: Option<String>,
    pub events: Vec<Line>,
    pub outputs: Vec<Line>,
}

pub fn parse(text: &str, origin: &str) -> Result<RecordedSession> {
    let mut meta = None;
    let (mut events, mut outputs) = (Vec::new(), Vec::new());
    for (i, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let line: Line = serde_json::from_str(raw).with_context(|| format!("{origin}:{}: invalid line", i + 1))?;
        match line {
            Line::Session { .. } => {
                if meta.replace(line).is_some() {
                    bail!("{origin}: more than one session line");
                }
            }
            Line::Event { .. } => events.push(line),
            Line::Output { .. } => outputs.push(line),
        }
    }
    let Some(Line::Session { external_session_id, project_key, cwd, harness, device_id }) = meta else {
        bail!("{origin}: no session line");
    };
    events.sort_by_key(|e| match e {
        Line::Event { seq, .. } => *seq,
        _ => 0,
    });
    Ok(RecordedSession { external_session_id, project_key, cwd, harness, device_id, events, outputs })
}

pub fn load(path: &Path) -> Result<RecordedSession> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text, &path.display().to_string())
}

/// Every `*.jsonl` file of `dir`, sorted by name.
pub fn load_dir(dir: &Path) -> Result<Vec<RecordedSession>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    paths.sort();
    paths.iter().map(|p| load(p)).collect()
}

pub fn to_text(lines: &[Line]) -> String {
    let mut out = String::new();
    for l in lines {
        out.push_str(&serde_json::to_string(l).expect("plain data serializes"));
        out.push('\n');
    }
    out
}
