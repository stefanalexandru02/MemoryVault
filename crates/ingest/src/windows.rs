//! Window assignment and closing (spec 9.1), and the deferred summarize packets.
//!
//! State lives in `session_windows` (migration 0100); the `sessions` row is the lock every change
//! takes ([`crate::builders::lock_session`]).
//!
//! - `window_no` starts at 1. A window closes on its `window_tool_uses`-th (8) `tool_use` /
//!   `tool_failure` event, on an `assistant_message` (Stop), on `POST /v1/sessions/{id}/end`, or
//!   when the worker sweep finds it idle for `window_idle_seconds` (90 s). Exactly one rule closes
//!   each window (`close_reason`).
//! - Closing a window with content creates `extract:<session>:<window>`. Windows holding only
//!   `user_prompt` / `system` events create no packet (and are never closed by the idle sweep, so a
//!   prompt is not cut off from the tool calls that follow it).
//! - A Stop owes `summarize_prompt:<session>:<prompt_no>` and `end` owes
//!   `summarize_session:<session>`. Packet inputs are immutable, so these are created once the
//!   packets they summarise have settled (done/failed/cancelled): the prompt summary then carries
//!   the window observations' titles and the session summary the prompt summaries.
//!   [`after_packet_settled`] and the sweep trigger that.
//! - Once a session has ended and its extract packets have settled, a scoped planner run is queued for
//!   every scope the session wrote observations in (`jobs(type='plan')`, trigger `session_end`).

use chrono::{DateTime, Utc};
use memvault_core::Settings;
use memvault_core::jobs::enqueue_plan;
use memvault_core::prompts::PromptRegistry;
use memvault_core::schemas::common::{EventKind, PacketKind, PlanTrigger, Scope, ScopeKind};
use memvault_core::system_tx;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::builders::{self, SessionRow, create_session_packet};

/// Statuses after which a packet no longer changes what it summarises.
const SETTLED: [&str; 3] = ["done", "failed", "cancelled"];

fn is_tool(kind: EventKind) -> bool {
    matches!(kind, EventKind::ToolUse | EventKind::ToolFailure)
}

fn is_content(kind: EventKind) -> bool {
    !matches!(kind, EventKind::UserPrompt | EventKind::System)
}

/// A newly inserted event to assign.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NewEvent {
    pub id: Uuid,
    pub seq: i32,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assignment {
    /// Window of the newest assigned event (the open window when nothing was assigned).
    pub window_no: i32,
    /// Windows closed by this call.
    pub closed: Vec<i32>,
    /// Kinds of packets created.
    pub packets: Vec<PacketKind>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct WindowRow {
    window_no: i32,
    prompt_no: i32,
    tool_uses: i32,
    event_count: i32,
    has_content: bool,
}

/// The session's open window, created (numbered after the last one) when there is none.
async fn open_window(conn: &mut PgConnection, session: &SessionRow) -> Result<WindowRow, sqlx::Error> {
    const COLS: &str = "window_no, prompt_no, tool_uses, event_count, has_content";
    let open: Option<WindowRow> =
        sqlx::query_as(&format!("SELECT {COLS} FROM session_windows WHERE session_id = $1 AND closed_at IS NULL"))
            .bind(session.id)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(w) = open {
        return Ok(w);
    }
    let last: Option<(i32, i32)> = sqlx::query_as(
        "SELECT window_no, prompt_no FROM session_windows WHERE session_id = $1 ORDER BY window_no DESC LIMIT 1",
    )
    .bind(session.id)
    .fetch_optional(&mut *conn)
    .await?;
    let (window_no, prompt_no) = last.map_or((1, 0), |(w, p)| (w + 1, p));
    sqlx::query_as(&format!(
        "INSERT INTO session_windows (space_id, session_id, window_no, prompt_no) VALUES ($1, $2, $3, $4)
         RETURNING {COLS}"
    ))
    .bind(session.space_id)
    .bind(session.id)
    .bind(window_no)
    .bind(prompt_no)
    .fetch_one(conn)
    .await
}

/// Assign newly inserted events (any order; processed by `seq`) to windows, closing windows as the
/// rules fire. The caller holds the session lock.
pub async fn assign_events(
    conn: &mut PgConnection,
    prompts: Option<&PromptRegistry>,
    settings: &Settings,
    session: &SessionRow,
    events: &[NewEvent],
) -> Result<Assignment, sqlx::Error> {
    let mut w = open_window(conn, session).await?;
    let mut result = Assignment { window_no: w.window_no, ..Assignment::default() };
    let mut sorted = events.to_vec();
    sorted.sort_by_key(|e| e.seq);
    let mut segment: Vec<Uuid> = Vec::new();
    for ev in sorted {
        segment.push(ev.id);
        w.event_count += 1;
        result.window_no = w.window_no;
        if ev.kind == EventKind::UserPrompt {
            w.prompt_no += 1;
        }
        if is_tool(ev.kind) {
            w.tool_uses += 1;
        }
        if is_content(ev.kind) {
            w.has_content = true;
        }
        let reason = if ev.kind == EventKind::AssistantMessage {
            "stop"
        } else if w.tool_uses >= settings.window_tool_uses {
            "tool_uses"
        } else {
            continue;
        };
        flush(conn, session, &w, &mut segment).await?;
        result.packets.extend(close_window(conn, prompts, session, w.window_no, reason).await?);
        result.closed.push(w.window_no);
        w = open_window(conn, session).await?;
    }
    if !segment.is_empty() {
        flush(conn, session, &w, &mut segment).await?;
    }
    Ok(result)
}

async fn flush(
    conn: &mut PgConnection,
    session: &SessionRow,
    w: &WindowRow,
    segment: &mut Vec<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE events SET window_no = $3 WHERE session_id = $1 AND id = ANY($2)")
        .bind(session.id)
        .bind(&*segment)
        .bind(w.window_no)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE session_windows SET prompt_no = $3, tool_uses = $4, event_count = $5, has_content = $6,
                last_event_at = now()
          WHERE session_id = $1 AND window_no = $2",
    )
    .bind(session.id)
    .bind(w.window_no)
    .bind(w.prompt_no)
    .bind(w.tool_uses)
    .bind(w.event_count)
    .bind(w.has_content)
    .execute(conn)
    .await?;
    segment.clear();
    Ok(())
}

/// Close one open window; returns the kinds of packets created. The caller holds the session lock.
pub async fn close_window(
    conn: &mut PgConnection,
    prompts: Option<&PromptRegistry>,
    session: &SessionRow,
    window_no: i32,
    reason: &str,
) -> Result<Vec<PacketKind>, sqlx::Error> {
    let closed: Option<(bool, i32)> = sqlx::query_as(
        "UPDATE session_windows SET closed_at = now(), close_reason = $3, prompt_summary_pending = ($3 = 'stop')
          WHERE session_id = $1 AND window_no = $2 AND closed_at IS NULL
         RETURNING has_content, prompt_no",
    )
    .bind(session.id)
    .bind(window_no)
    .bind(reason)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((has_content, prompt_no)) = closed else {
        return Ok(Vec::new());
    };
    metrics::counter!("memsvc_windows_closed_total", "reason" => reason.to_string()).increment(1);
    let mut created = Vec::new();
    if has_content {
        let key = format!("extract:{}:{window_no}", session.id);
        let inputs = builders::build_extract_inputs(conn, session, window_no).await?;
        if create_session_packet(conn, prompts, session, PacketKind::Extract, &key, &inputs).await? {
            created.push(PacketKind::Extract);
        }
        sqlx::query(
            "UPDATE session_windows SET extract_packet_id = (SELECT id FROM work_packets WHERE idempotency_key = $3)
              WHERE session_id = $1 AND window_no = $2",
        )
        .bind(session.id)
        .bind(window_no)
        .bind(&key)
        .execute(&mut *conn)
        .await?;
    }
    if reason == "stop" {
        created.extend(maybe_create_prompt_summary(conn, prompts, session, prompt_no).await?);
    }
    Ok(created)
}

/// Create the owed `summarize_prompt` packet once the prompt's extract packets have settled.
pub async fn maybe_create_prompt_summary(
    conn: &mut PgConnection,
    prompts: Option<&PromptRegistry>,
    session: &SessionRow,
    prompt_no: i32,
) -> Result<Vec<PacketKind>, sqlx::Error> {
    let windows: Vec<(i32, bool, Option<String>)> = sqlx::query_as(
        "SELECT w.window_no, w.prompt_summary_pending, p.status
           FROM session_windows w LEFT JOIN work_packets p ON p.id = w.extract_packet_id
          WHERE w.session_id = $1 AND w.prompt_no = $2 AND w.closed_at IS NOT NULL",
    )
    .bind(session.id)
    .bind(prompt_no)
    .fetch_all(&mut *conn)
    .await?;
    if !windows.iter().any(|(_, pending, _)| *pending) {
        return Ok(Vec::new());
    }
    if windows.iter().any(|(_, _, status)| status.as_deref().is_some_and(|s| !SETTLED.contains(&s))) {
        return Ok(Vec::new());
    }
    let window_nos: Vec<i32> = windows.iter().map(|(w, _, _)| *w).collect();
    sqlx::query(
        "UPDATE session_windows SET prompt_summary_pending = false WHERE session_id = $1 AND window_no = ANY($2)",
    )
    .bind(session.id)
    .bind(&window_nos)
    .execute(&mut *conn)
    .await?;
    let inputs = builders::build_summarize_prompt_inputs(conn, session, prompt_no, &window_nos).await?;
    let key = format!("summarize_prompt:{}:{prompt_no}", session.id);
    let created = create_session_packet(conn, prompts, session, PacketKind::SummarizePrompt, &key, &inputs).await?;
    Ok(if created { vec![PacketKind::SummarizePrompt] } else { Vec::new() })
}

/// Once `session` has ended and none of its extract packets is in flight, queue a planner run for each
/// scope it wrote observations in.
async fn plan_ended_session(conn: &mut PgConnection, session: &SessionRow) -> Result<(), sqlx::Error> {
    let extracted: bool = sqlx::query_scalar(
        "SELECT s.ended_at IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM work_packets WHERE session_id = s.id AND kind = 'extract'
                                                         AND status IN ('pending', 'leased'))
           FROM sessions s WHERE s.id = $1",
    )
    .bind(session.id)
    .fetch_one(&mut *conn)
    .await?;
    if !extracted {
        return Ok(());
    }
    let scopes: Vec<(ScopeKind, String)> = sqlx::query_as(
        "SELECT DISTINCT scope_kind, scope_key FROM observations WHERE session_id = $1 AND archived_at IS NULL",
    )
    .bind(session.id)
    .fetch_all(&mut *conn)
    .await?;
    for (kind, key) in scopes {
        enqueue_plan(conn, session.space_id, &Scope::new(kind, key), PlanTrigger::SessionEnd).await?;
    }
    Ok(())
}

/// Create the owed `summarize_session` packet once nothing it folds is still in flight.
pub async fn maybe_create_session_summary(
    conn: &mut PgConnection,
    prompts: Option<&PromptRegistry>,
    session: &SessionRow,
) -> Result<Vec<PacketKind>, sqlx::Error> {
    let pending: bool = sqlx::query_scalar("SELECT session_summary_pending FROM sessions WHERE id = $1")
        .bind(session.id)
        .fetch_one(&mut *conn)
        .await?;
    if !pending {
        return Ok(Vec::new());
    }
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM session_windows WHERE session_id = $1 AND prompt_summary_pending)
             OR EXISTS (SELECT 1 FROM work_packets WHERE session_id = $1
                          AND kind IN ('extract', 'summarize_prompt') AND status IN ('pending', 'leased'))",
    )
    .bind(session.id)
    .fetch_one(&mut *conn)
    .await?;
    if busy {
        return Ok(Vec::new());
    }
    sqlx::query("UPDATE sessions SET session_summary_pending = false WHERE id = $1")
        .bind(session.id)
        .execute(&mut *conn)
        .await?;
    let Some(inputs) = builders::build_summarize_session_inputs(conn, session).await? else {
        return Ok(Vec::new());
    };
    let key = format!("summarize_session:{}", session.id);
    let created = create_session_packet(conn, prompts, session, PacketKind::SummarizeSession, &key, &inputs).await?;
    Ok(if created { vec![PacketKind::SummarizeSession] } else { Vec::new() })
}

/// What `POST /v1/sessions/{id}/end` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub ended_at: DateTime<Utc>,
    /// An open window with events was closed.
    pub window_closed: bool,
    pub packets: Vec<PacketKind>,
}

/// Close the open window and owe the session summary. The caller holds the session lock.
pub async fn end_session(
    conn: &mut PgConnection,
    prompts: Option<&PromptRegistry>,
    session: &SessionRow,
    reason: Option<&str>,
    occurred_at: Option<DateTime<Utc>>,
) -> Result<Ended, sqlx::Error> {
    // A late (spooled) end reports when it happened, within the session's lifetime.
    let ended_at: DateTime<Utc> = sqlx::query_scalar(
        "UPDATE sessions SET ended_at = greatest(least(coalesce($3, now()), now()), started_at),
                end_reason = $2, session_summary_pending = true
          WHERE id = $1
         RETURNING ended_at",
    )
    .bind(session.id)
    .bind(reason)
    .bind(occurred_at)
    .fetch_one(&mut *conn)
    .await?;
    let open: Option<(i32, i32)> = sqlx::query_as(
        "SELECT window_no, event_count FROM session_windows WHERE session_id = $1 AND closed_at IS NULL",
    )
    .bind(session.id)
    .fetch_optional(&mut *conn)
    .await?;
    let mut packets = Vec::new();
    let mut window_closed = false;
    match open {
        Some((window_no, count)) if count > 0 => {
            packets.extend(close_window(conn, prompts, session, window_no, "end").await?);
            window_closed = true;
        }
        Some((window_no, _)) => {
            // an empty open window (left by the last close): drop it rather than close it empty
            sqlx::query("DELETE FROM session_windows WHERE session_id = $1 AND window_no = $2")
                .bind(session.id)
                .bind(window_no)
                .execute(&mut *conn)
                .await?;
        }
        None => {}
    }
    // The final prompt may never have seen a Stop (interrupted turn, stop_hook_active, idle close):
    // owe its summary now unless one was already requested.
    let last: Option<(i32, i32)> = sqlx::query_as(
        "SELECT window_no, prompt_no FROM session_windows
          WHERE session_id = $1 AND closed_at IS NOT NULL AND has_content AND prompt_no > 0
          ORDER BY window_no DESC LIMIT 1",
    )
    .bind(session.id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((window_no, prompt_no)) = last {
        let key = format!("summarize_prompt:{}:{prompt_no}", session.id);
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM work_packets WHERE idempotency_key = $1)")
            .bind(&key)
            .fetch_one(&mut *conn)
            .await?;
        if !exists {
            sqlx::query(
                "UPDATE session_windows SET prompt_summary_pending = true WHERE session_id = $1 AND window_no = $2",
            )
            .bind(session.id)
            .bind(window_no)
            .execute(&mut *conn)
            .await?;
            packets.extend(maybe_create_prompt_summary(conn, prompts, session, prompt_no).await?);
        }
    }
    packets.extend(maybe_create_session_summary(conn, prompts, session).await?);
    plan_ended_session(conn, session).await?;
    Ok(Ended { ended_at, window_closed, packets })
}

/// Call when an extract/summarize packet reached done/failed/cancelled, in the same transaction.
pub async fn after_packet_settled(
    conn: &mut PgConnection,
    prompts: Option<&PromptRegistry>,
    kind: PacketKind,
    session_id: Option<Uuid>,
    inputs: &Value,
) -> Result<Vec<PacketKind>, sqlx::Error> {
    let Some(session_id) = session_id.filter(|_| matches!(kind, PacketKind::Extract | PacketKind::SummarizePrompt))
    else {
        return Ok(Vec::new());
    };
    let Some(session) = builders::lock_session(conn, session_id).await? else {
        return Ok(Vec::new());
    };
    let mut created = Vec::new();
    if kind == PacketKind::Extract {
        let window_no = inputs.get("window_no").and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok());
        let prompt_no: Option<i32> =
            sqlx::query_scalar("SELECT prompt_no FROM session_windows WHERE session_id = $1 AND window_no = $2")
                .bind(session_id)
                .bind(window_no)
                .fetch_optional(&mut *conn)
                .await?;
        if let Some(prompt_no) = prompt_no {
            created.extend(maybe_create_prompt_summary(conn, prompts, &session, prompt_no).await?);
        }
        plan_ended_session(conn, &session).await?;
    }
    created.extend(maybe_create_session_summary(conn, prompts, &session).await?);
    Ok(created)
}

/// A system transaction for one unit of sweep work. The lock timeout turns a wait on a busy row
/// into an error for that unit alone.
pub(crate) async fn sweep_tx(pool: &PgPool) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, sqlx::Error> {
    let mut tx = system_tx(pool).await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'").execute(&mut *tx).await?;
    Ok(tx)
}

/// Close every open window with content whose newest event is older than
/// `settings.window_idle_seconds` (spec 9.1) and create its `extract` packet. Called by the worker
/// sweep on the system pool. Windows holding only `user_prompt` / `system` events stay open.
///
/// The same sweep fails expired leases that used their last attempt ([`crate::queue::expire_leases`])
/// and creates summarize packets whose prerequisites settled without passing through the result
/// route. Packets created here store an unpinned prompt ref (no registry at hand), pinned at lease.
///
/// Every session and every packet is handled in its own short transaction that skips rows other
/// transactions (a second worker replica, an event post, a result post) hold, so a failure or a
/// busy row costs one unit of work until the next tick and never holds locks across units. Returns
/// the number of windows closed.
pub async fn close_idle_windows(pool: &PgPool, settings: &Settings) -> anyhow::Result<u64> {
    let idle = settings.window_idle_seconds as f64;
    let mut tx = system_tx(pool).await?;
    let sessions: Vec<Uuid> = sqlx::query_scalar(
        "SELECT s.id FROM sessions s
          WHERE EXISTS (SELECT 1 FROM session_windows w WHERE w.session_id = s.id AND w.closed_at IS NULL
                          AND w.has_content AND w.last_event_at < now() - make_interval(secs => $1))
          ORDER BY s.id",
    )
    .bind(idle)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut closed = 0;
    for session_id in sessions {
        match close_idle_window(pool, session_id, idle).await {
            Ok(true) => closed += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(session = %session_id, error = %e, "closing an idle window failed"),
        }
    }

    crate::queue::expire_leases(pool, None).await?;

    let mut tx = system_tx(pool).await?;
    let owed: Vec<(Uuid, Option<i32>)> = sqlx::query_as(
        "SELECT DISTINCT session_id, prompt_no FROM session_windows WHERE prompt_summary_pending
         UNION ALL
         SELECT id, NULL FROM sessions WHERE session_summary_pending",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for (session_id, prompt_no) in owed {
        if let Err(e) = create_owed_summary(pool, session_id, prompt_no).await {
            tracing::warn!(session = %session_id, error = %e, "creating an owed summary failed");
        }
    }
    Ok(closed)
}

/// Close the idle window of one session, unless another transaction holds the session.
async fn close_idle_window(pool: &PgPool, session_id: Uuid, idle: f64) -> Result<bool, sqlx::Error> {
    let mut tx = sweep_tx(pool).await?;
    let Some(session) = builders::try_lock_session(&mut tx, session_id).await? else {
        return Ok(false);
    };
    let window: Option<i32> = sqlx::query_scalar(
        "SELECT window_no FROM session_windows WHERE session_id = $1 AND closed_at IS NULL
            AND has_content AND last_event_at < now() - make_interval(secs => $2)",
    )
    .bind(session_id)
    .bind(idle)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(window_no) = window else {
        return Ok(false);
    };
    close_window(&mut tx, None, &session, window_no, "idle").await?;
    tx.commit().await?;
    Ok(true)
}

async fn create_owed_summary(pool: &PgPool, session_id: Uuid, prompt_no: Option<i32>) -> Result<(), sqlx::Error> {
    let mut tx = sweep_tx(pool).await?;
    let Some(session) = builders::try_lock_session(&mut tx, session_id).await? else {
        return Ok(());
    };
    match prompt_no {
        Some(n) => maybe_create_prompt_summary(&mut tx, None, &session, n).await?,
        None => maybe_create_session_summary(&mut tx, None, &session).await?,
    };
    tx.commit().await?;
    Ok(())
}
