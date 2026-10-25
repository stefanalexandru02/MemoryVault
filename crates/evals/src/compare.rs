//! `memvault evals compare`: claude-mem's SQLite and this service for the same session ids
//! (spec 16.3).
//!
//! Sessions are matched on claude-mem's `content_session_id` = our `external_session_id`.
//! claude-mem has no token estimate, so its observation tokens are text length / 4, the same
//! estimate this service stores.

use std::path::Path;

use anyhow::{Context, Result};
use memvault_core::{AppState, system_tx};
use rusqlite::{Connection, OpenFlags, params, types::Value};
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Side {
    pub observations: i64,
    pub observation_tokens: i64,
    pub summaries: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub session: String,
    pub claude_mem: Side,
    pub memvault: Side,
    /// Facts (active or candidate) citing this session's observations; claude-mem has none.
    pub memvault_facts: i64,
}

/// Columns of claude-mem's `observations` that carry text.
const TEXT_COLUMNS: [&str; 5] = ["title", "subtitle", "narrative", "text", "facts"];

fn table_columns(db: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<Vec<_>, _>>()?;
    Ok(names)
}

fn claude_mem_side(db: &Connection, session: &str) -> Result<Side> {
    let memory_ids: Vec<String> = {
        let mut stmt =
            db.prepare("SELECT memory_session_id FROM sdk_sessions WHERE content_session_id = ?1 AND memory_session_id IS NOT NULL")?;
        stmt.query_map(params![session], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    let columns = table_columns(db, "observations")?;
    let text_cols: Vec<&str> = TEXT_COLUMNS.iter().copied().filter(|c| columns.iter().any(|x| x == c)).collect();
    let mut side = Side::default();
    for id in memory_ids {
        if !text_cols.is_empty() {
            let mut stmt =
                db.prepare(&format!("SELECT {} FROM observations WHERE memory_session_id = ?1", text_cols.join(", ")))?;
            let mut rows = stmt.query(params![id])?;
            while let Some(row) = rows.next()? {
                side.observations += 1;
                let mut chars = 0;
                for i in 0..text_cols.len() {
                    if let Value::Text(t) = row.get::<_, Value>(i)? {
                        chars += t.chars().count() as i64;
                    }
                }
                side.observation_tokens += chars / 4;
            }
        }
        if !table_columns(db, "session_summaries")?.is_empty() {
            side.summaries += db.query_row(
                "SELECT count(*) FROM session_summaries WHERE memory_session_id = ?1",
                params![id],
                |r| r.get::<_, i64>(0),
            )?;
        }
    }
    Ok(side)
}

async fn memvault_side(state: &AppState, session: &str) -> Result<(Side, i64)> {
    let mut tx = system_tx(&state.db.system).await?;
    let (observations, observation_tokens): (i64, i64) = sqlx::query_as(
        "SELECT count(*), coalesce(sum(o.token_estimate), 0)::bigint
           FROM observations o JOIN sessions s ON s.id = o.session_id
          WHERE s.external_session_id = $1 AND o.archived_at IS NULL",
    )
    .bind(session)
    .fetch_one(&mut *tx)
    .await?;
    let summaries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM summaries m JOIN sessions s ON s.id = m.session_id
          WHERE s.external_session_id = $1 AND m.grain = 'session'",
    )
    .bind(session)
    .fetch_one(&mut *tx)
    .await?;
    let facts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM facts f
          WHERE f.status IN ('active','candidate') AND f.evidence_observation_ids &&
                (SELECT coalesce(array_agg(o.id), '{}') FROM observations o JOIN sessions s ON s.id = o.session_id
                  WHERE s.external_session_id = $1)",
    )
    .bind(session)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok((Side { observations, observation_tokens, summaries }, facts))
}

pub async fn compare(state: &AppState, claude_mem_db: &Path, sessions: &[String]) -> Result<Vec<Row>> {
    let db = Connection::open_with_flags(claude_mem_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", claude_mem_db.display()))?;
    let mut rows = Vec::new();
    for s in sessions {
        let (memvault, memvault_facts) = memvault_side(state, s).await?;
        rows.push(Row { session: s.clone(), claude_mem: claude_mem_side(&db, s)?, memvault, memvault_facts });
    }
    Ok(rows)
}

pub fn print(rows: &[Row]) {
    println!(
        "{:<28} {:>8} {:>8} {:>6}   {:>8} {:>8} {:>6} {:>6}",
        "session", "cm obs", "cm tok", "cm sum", "mv obs", "mv tok", "mv sum", "facts"
    );
    let mut total =
        Row { session: "total".into(), claude_mem: Side::default(), memvault: Side::default(), memvault_facts: 0 };
    for r in rows.iter() {
        let name: String = r.session.chars().take(28).collect();
        println!(
            "{:<28} {:>8} {:>8} {:>6}   {:>8} {:>8} {:>6} {:>6}",
            name,
            r.claude_mem.observations,
            r.claude_mem.observation_tokens,
            r.claude_mem.summaries,
            r.memvault.observations,
            r.memvault.observation_tokens,
            r.memvault.summaries,
            r.memvault_facts
        );
        total.claude_mem.observations += r.claude_mem.observations;
        total.claude_mem.observation_tokens += r.claude_mem.observation_tokens;
        total.claude_mem.summaries += r.claude_mem.summaries;
        total.memvault.observations += r.memvault.observations;
        total.memvault.observation_tokens += r.memvault.observation_tokens;
        total.memvault.summaries += r.memvault.summaries;
        total.memvault_facts += r.memvault_facts;
    }
    println!(
        "{:<28} {:>8} {:>8} {:>6}   {:>8} {:>8} {:>6} {:>6}",
        total.session,
        total.claude_mem.observations,
        total.claude_mem.observation_tokens,
        total.claude_mem.summaries,
        total.memvault.observations,
        total.memvault.observation_tokens,
        total.memvault.summaries,
        total.memvault_facts
    );
}
