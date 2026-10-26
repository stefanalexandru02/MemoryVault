//! Session rows, the prompt a session packet is built with, and packet inputs for extract /
//! summarize packets (spec 9.2), built once at creation.

use chrono::{DateTime, Utc};
use memvault_core::estimate_tokens;
use memvault_core::prompts::PromptRegistry;
use memvault_core::schemas::common::{EventKind, OutputSchemaName, PacketKind};
use memvault_core::schemas::packets::{
    ExtractInputs, PacketEvent, PacketSessionInfo, PromptSummaryIn, RecentTitle, SummarizePromptInputs,
    SummarizeSessionInputs,
};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::queue::{NewPacket, create_packet};

/// ~400 tokens (`estimate_tokens` = chars / 4).
pub const PROFILE_MAX_CHARS: usize = 1600;
pub const RECENT_TITLES: i64 = 20;
pub const KNOWN_ENTITIES: i64 = 50;

/// A `sessions` row joined with its client.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionRow {
    pub id: Uuid,
    pub space_id: Uuid,
    pub client_id: Uuid,
    pub harness: String,
    pub project_key: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub device_id: String,
    pub platform: Option<String>,
}

const SESSION_SQL: &str = "SELECT s.id, s.space_id, s.client_id, s.harness, s.project_key, s.started_at, s.ended_at,
                                  c.device_id, c.platform
                             FROM sessions s JOIN clients c ON c.id = s.client_id
                            WHERE s.id = $1";

pub async fn session_row(conn: &mut PgConnection, session_id: Uuid) -> Result<Option<SessionRow>, sqlx::Error> {
    sqlx::query_as(SESSION_SQL).bind(session_id).fetch_optional(conn).await
}

/// Lock the session row (the mutex for every change to the session's windows) and return it.
pub async fn lock_session(conn: &mut PgConnection, session_id: Uuid) -> Result<Option<SessionRow>, sqlx::Error> {
    sqlx::query_as(&format!("{SESSION_SQL} FOR UPDATE OF s")).bind(session_id).fetch_optional(conn).await
}

/// [`lock_session`] that skips a row another transaction holds (`None` for that and for a missing
/// session alike): the sweep leaves a busy session to the next tick instead of waiting on it.
pub async fn try_lock_session(conn: &mut PgConnection, session_id: Uuid) -> Result<Option<SessionRow>, sqlx::Error> {
    sqlx::query_as(&format!("{SESSION_SQL} FOR UPDATE OF s SKIP LOCKED")).bind(session_id).fetch_optional(conn).await
}

pub(crate) fn to_json<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("plain data serializes to JSON")
}

/// The prompt a packet is built with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptChoice {
    pub prompt_ref: String,
    pub model_hint: String,
    pub output_schema: OutputSchemaName,
}

/// Prompt names per session packet kind; the first loaded name wins.
fn prompt_names(kind: PacketKind) -> &'static [&'static str] {
    match kind {
        PacketKind::Extract => &["observer.code"],
        PacketKind::SummarizePrompt => &["summary.prompt", "summary"],
        PacketKind::SummarizeSession => &["summary.session", "summary"],
        _ => &[],
    }
}

fn default_schema(kind: PacketKind) -> OutputSchemaName {
    match kind {
        PacketKind::Extract => OutputSchemaName::ObserverV1,
        PacketKind::ConsolidateFacts => OutputSchemaName::ConsolidateFactsV1,
        PacketKind::ConsolidateSummary => OutputSchemaName::ConsolidateSummaryV1,
        PacketKind::ConsolidateProcedures => OutputSchemaName::ConsolidateProceduresV1,
        PacketKind::SummarizePrompt | PacketKind::SummarizeSession => OutputSchemaName::SummaryV1,
    }
}

/// Pinned `name@version` of the newest loaded prompt for `kind`. Without a registry (the worker
/// sweep) or a loaded prompt, the bare name is stored and pinned at lease time instead, so ingest
/// never fails because no prompt is loaded.
pub fn prompt_for(prompts: Option<&PromptRegistry>, kind: PacketKind) -> PromptChoice {
    let names = prompt_names(kind);
    if let Some(spec) = prompts.and_then(|reg| names.iter().find_map(|n| reg.get(n, None).ok())) {
        return PromptChoice {
            prompt_ref: spec.prompt_ref(),
            model_hint: spec.model_hint.clone(),
            output_schema: spec.output_schema,
        };
    }
    if prompts.is_some() {
        tracing::warn!(kind = %kind, "no prompt loaded for packet kind; storing an unpinned ref");
    }
    PromptChoice {
        prompt_ref: names.first().copied().unwrap_or(kind.as_str()).to_string(),
        model_hint: "sonnet".into(),
        output_schema: default_schema(kind),
    }
}

/// Create an extract/summarize packet for `session`; its client is the preferred runner (9.3).
/// Returns false when the idempotency key already exists.
pub async fn create_session_packet(
    conn: &mut PgConnection,
    prompts: Option<&PromptRegistry>,
    session: &SessionRow,
    kind: PacketKind,
    idempotency_key: &str,
    inputs: &Value,
) -> Result<bool, sqlx::Error> {
    let choice = prompt_for(prompts, kind);
    let id = create_packet(
        conn,
        NewPacket {
            space_id: session.space_id,
            kind,
            prompt_ref: &choice.prompt_ref,
            model_hint: &choice.model_hint,
            output_schema: choice.output_schema,
            inputs,
            idempotency_key,
            session_id: Some(session.id),
            preferred_client_id: Some(session.client_id),
            max_attempts: 3,
        },
    )
    .await?;
    Ok(id.is_some())
}

fn session_info(session: &SessionRow) -> PacketSessionInfo {
    PacketSessionInfo {
        harness: session.harness.clone(),
        project_key: session.project_key.clone(),
        device: Some(session.device_id.clone()),
        started_at: session.started_at,
    }
}

fn text_field(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(Value::as_str).map(String::from)
}

/// One `events` row as packet input. Null fields are dropped; `agent_id` / `agent_type` are added
/// for subagent events (spec 8.3: the extractor tags their observations `subagent:<agent_type>`).
pub fn packet_event(seq: i32, kind: EventKind, at: DateTime<Utc>, payload: &Value) -> Value {
    let field = |key: &str| payload.get(key).cloned().unwrap_or(Value::Null);
    let mut ev = PacketEvent {
        seq,
        kind: kind.as_str().to_string(),
        at,
        tool: None,
        input: Value::Null,
        output: Value::Null,
        text: None,
    };
    match kind {
        EventKind::ToolUse | EventKind::ToolResult => {
            ev.tool = text_field(payload, "tool_name");
            ev.input = field("tool_input");
            ev.output = field("tool_response");
        }
        EventKind::ToolFailure => {
            ev.tool = text_field(payload, "tool_name");
            ev.input = field("tool_input");
            ev.output = field("error");
        }
        EventKind::UserPrompt => ev.text = text_field(payload, "prompt"),
        EventKind::AssistantMessage | EventKind::SubagentStop => {
            ev.text = text_field(payload, "last_assistant_message");
        }
        EventKind::System | EventKind::Note => {
            ev.text = text_field(payload, "text");
            if ev.text.is_none() && payload.as_object().is_some_and(|m| !m.is_empty()) {
                ev.input = payload.clone();
            }
        }
    }
    let mut out = to_json(&ev);
    let map = out.as_object_mut().expect("PacketEvent is an object");
    map.retain(|_, v| !v.is_null());
    for key in ["agent_id", "agent_type"] {
        if let Some(v) = payload.get(key).filter(|v| !v.is_null() && v.as_str() != Some("")) {
            map.insert(key.into(), v.clone());
        }
    }
    out
}

/// Estimated tokens of the events a summary covers, each as an extraction window shows it ([`packet_event`]): the
/// windows of prompt `prompt_no`, or the whole session (`None`). `None` when there are none.
pub async fn summary_work_tokens(
    conn: &mut PgConnection,
    session_id: Uuid,
    prompt_no: Option<i32>,
) -> Result<Option<i32>, sqlx::Error> {
    let events: Vec<(i32, EventKind, DateTime<Utc>, Value)> = sqlx::query_as(
        "SELECT seq, kind, occurred_at, payload FROM events
          WHERE session_id = $1
            AND ($2::int IS NULL
                 OR window_no IN (SELECT window_no FROM session_windows WHERE session_id = $1 AND prompt_no = $2))",
    )
    .bind(session_id)
    .bind(prompt_no)
    .fetch_all(conn)
    .await?;
    Ok(events
        .iter()
        .map(|(seq, kind, at, payload)| estimate_tokens(&packet_event(*seq, *kind, *at, payload).to_string()))
        .reduce(i32::saturating_add))
}

fn to_titles(rows: Vec<(Uuid, String, String)>) -> Vec<RecentTitle> {
    rows.into_iter().map(|(id, obs_type, title)| RecentTitle { id, obs_type, title }).collect()
}

pub async fn build_extract_inputs(
    conn: &mut PgConnection,
    session: &SessionRow,
    window_no: i32,
) -> Result<Value, sqlx::Error> {
    let events: Vec<(i32, EventKind, DateTime<Utc>, Value)> = sqlx::query_as(
        "SELECT seq, kind, occurred_at, payload FROM events WHERE session_id = $1 AND window_no = $2 ORDER BY seq",
    )
    .bind(session.id)
    .bind(window_no)
    .fetch_all(&mut *conn)
    .await?;
    let recent_titles = to_titles(
        sqlx::query_as(
            "SELECT id, type, title FROM (
               SELECT id, type, title, created_at FROM observations
                WHERE session_id = $1 AND archived_at IS NULL ORDER BY created_at DESC, id DESC LIMIT $2) t
              ORDER BY created_at, id",
        )
        .bind(session.id)
        .bind(RECENT_TITLES)
        .fetch_all(&mut *conn)
        .await?,
    );
    let profile: Option<String> = sqlx::query_scalar(
        "SELECT body_md FROM profiles WHERE space_id = $1
            AND ((scope_kind = 'project' AND scope_key = $2) OR scope_kind = 'global')
          ORDER BY (scope_kind = 'project') DESC LIMIT 1",
    )
    .bind(session.space_id)
    .bind(session.project_key.as_deref().unwrap_or(""))
    .fetch_optional(&mut *conn)
    .await?;
    let known_entities: Vec<String> =
        sqlx::query_scalar("SELECT canonical_name FROM entities WHERE space_id = $1 ORDER BY created_at DESC LIMIT $2")
            .bind(session.space_id)
            .bind(KNOWN_ENTITIES)
            .fetch_all(&mut *conn)
            .await?;
    let inputs = ExtractInputs {
        mode: "code".into(),
        session: session_info(session),
        window_no,
        events: Vec::new(),
        recent_titles,
        profile_md: profile.unwrap_or_default().chars().take(PROFILE_MAX_CHARS).collect(),
        known_entities,
    };
    let mut out = to_json(&inputs);
    out["events"] = events.iter().map(|(seq, kind, at, payload)| packet_event(*seq, *kind, *at, payload)).collect();
    Ok(out)
}

pub async fn build_summarize_prompt_inputs(
    conn: &mut PgConnection,
    session: &SessionRow,
    prompt_no: i32,
    window_nos: &[i32],
) -> Result<Value, sqlx::Error> {
    let mut prompt = None;
    if prompt_no > 0 {
        let payload: Option<Value> = sqlx::query_scalar(
            "SELECT payload FROM events WHERE session_id = $1 AND kind = 'user_prompt' ORDER BY seq OFFSET $2 LIMIT 1",
        )
        .bind(session.id)
        .bind(i64::from(prompt_no - 1))
        .fetch_optional(&mut *conn)
        .await?;
        prompt = payload.and_then(|p| text_field(&p, "prompt"));
    }
    let last: Option<Value> = sqlx::query_scalar(
        "SELECT payload FROM events WHERE session_id = $1 AND window_no = ANY($2)
            AND kind = 'assistant_message' ORDER BY seq DESC LIMIT 1",
    )
    .bind(session.id)
    .bind(window_nos)
    .fetch_optional(&mut *conn)
    .await?;
    let observation_titles = to_titles(
        sqlx::query_as(
            "SELECT id, type, title FROM observations
              WHERE session_id = $1 AND window_no = ANY($2) AND archived_at IS NULL ORDER BY created_at, id",
        )
        .bind(session.id)
        .bind(window_nos)
        .fetch_all(&mut *conn)
        .await?,
    );
    Ok(to_json(&SummarizePromptInputs {
        mode: "code".into(),
        session: session_info(session),
        prompt_no,
        prompt,
        observation_titles,
        last_assistant_message: last.and_then(|p| text_field(&p, "last_assistant_message")),
    }))
}

type PromptSummaryRow =
    (Option<i32>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);

/// `None` when the session has no prompt summaries (nothing to fold).
pub async fn build_summarize_session_inputs(
    conn: &mut PgConnection,
    session: &SessionRow,
) -> Result<Option<Value>, sqlx::Error> {
    let rows: Vec<PromptSummaryRow> = sqlx::query_as(
        "SELECT prompt_no, request, investigated, learned, completed, next_steps, notes FROM summaries
          WHERE session_id = $1 AND grain = 'prompt' ORDER BY prompt_no NULLS LAST, created_at",
    )
    .bind(session.id)
    .fetch_all(conn)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let prompt_summaries = rows
        .into_iter()
        .map(|(prompt_no, request, investigated, learned, completed, next_steps, notes)| PromptSummaryIn {
            prompt_no,
            request,
            investigated,
            learned,
            completed,
            next_steps,
            notes,
        })
        .collect();
    Ok(Some(to_json(&SummarizeSessionInputs { mode: "code".into(), session: session_info(session), prompt_summaries })))
}
