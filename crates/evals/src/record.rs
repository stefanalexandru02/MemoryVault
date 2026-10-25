//! `memvault evals record`: export a session's events and the outputs its packets received to
//! `evals/sessions/<id>.jsonl` (spec 16.2). Run `redact` on the file before committing it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use memvault_core::{AppState, system_tx};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::session::{Line, to_text};

#[derive(sqlx::FromRow)]
struct Meta {
    external_session_id: String,
    project_key: Option<String>,
    cwd: Option<String>,
    harness: String,
    device_id: String,
}

/// Read the session `session` (a session id, or an `external_session_id`) and write
/// `<out_dir>/<session id>.jsonl`. Returns the path.
pub async fn record(state: &AppState, session: &str, out_dir: &Path) -> Result<PathBuf> {
    let mut tx = system_tx(&state.db.system).await?;
    let id: Option<Uuid> = match session.parse::<Uuid>() {
        Ok(id) => Some(id),
        Err(_) => {
            let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM sessions WHERE external_session_id = $1")
                .bind(session)
                .fetch_all(&mut *tx)
                .await?;
            match ids.as_slice() {
                [] => None,
                [one] => Some(*one),
                _ => bail!("external session id {session:?} matches {} sessions; pass the session id", ids.len()),
            }
        }
    };
    let id = id.with_context(|| format!("no session {session:?}"))?;
    let row: Option<Meta> = sqlx::query_as(
        "SELECT s.external_session_id, s.project_key, s.cwd, s.harness, c.device_id
           FROM sessions s JOIN clients c ON c.id = s.client_id WHERE s.id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let m = row.with_context(|| format!("no session {session:?}"))?;

    let mut lines = vec![Line::Session {
        external_session_id: m.external_session_id,
        project_key: m.project_key,
        cwd: m.cwd,
        harness: m.harness,
        device_id: Some(m.device_id),
    }];

    let events: Vec<(i32, String, DateTime<Utc>, Value)> =
        sqlx::query_as("SELECT seq, kind, occurred_at, payload FROM events WHERE session_id = $1 ORDER BY seq")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    for (seq, kind, occurred_at, payload) in events {
        let payload = match payload {
            Value::Object(m) => m,
            other => Map::from_iter([("value".to_string(), other)]),
        };
        lines.push(Line::Event { seq, kind, occurred_at, payload });
    }

    let packets: Vec<(String, Value, Value)> = sqlx::query_as(
        "SELECT kind, inputs, result FROM work_packets
          WHERE session_id = $1 AND status = 'done' AND result IS NOT NULL
            AND kind IN ('extract','summarize_prompt','summarize_session')
          ORDER BY created_at, id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    for (kind, inputs, output) in packets {
        lines.push(Line::Output {
            window_no: (kind == "extract").then(|| inputs["window_no"].as_i64()).flatten(),
            prompt_no: (kind == "summarize_prompt").then(|| inputs["prompt_no"].as_i64()).flatten(),
            packet: kind,
            output,
        });
    }

    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let path = out_dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, to_text(&lines)).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}
