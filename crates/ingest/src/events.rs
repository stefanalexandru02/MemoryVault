//! Spec 6.2, 9.1: `POST /v1/sessions/{id}/events` (events:write): clamp, idempotent insert, window
//! assignment and extract packets in the same transaction.
//!
//! Every event is inserted with `ON CONFLICT DO NOTHING` (`idempotency_key` and `(session_id, seq)`
//! are unique), so replaying a spool file is a no-op that reports duplicates.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use memvault_core::auth;
use memvault_core::http::{Json, Path};
use memvault_core::ids::new_id;
use memvault_core::schemas::events::{
    EventsAccepted, EventsBatch, FIELD_HEAD_CHARS, FIELD_TAIL_CHARS, PAYLOAD_CAP_BYTES,
};
use memvault_core::{ApiError, AppState, Identity, estimate_tokens, require_scope};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::sessions::lock_own_session;
use crate::windows::{self, NewEvent};
use memvault_core::user_tx;

/// The per-string limits are halved down to this before the excerpt fallback.
const MIN_HEAD_CHARS: usize = 64;
/// Last-resort excerpt; even fully escaped it stays far below the 64 KB cap.
const EXCERPT_CHARS: usize = 8000;
/// Top-level strings at most this long survive the excerpt fallback.
const KEEP_SCALAR_CHARS: usize = 256;

/// A string longer than `head + tail` chars becomes `head…[clamped N chars]…tail`.
pub fn clamp_str(s: &str, head: usize, tail: usize) -> Option<String> {
    let len = s.chars().count();
    if len <= head + tail {
        return None;
    }
    let cut = len - head - tail;
    let head_end = s.char_indices().nth(head).map_or(s.len(), |(i, _)| i);
    let tail_start = s.char_indices().nth(len - tail).map_or(s.len(), |(i, _)| i);
    Some(format!("{}…[clamped {cut} chars]…{}", &s[..head_end], &s[tail_start..]))
}

/// Clamp every string in `value`, recursively (object keys are kept).
pub fn clamp_strings(value: &Value, head: usize, tail: usize) -> Value {
    match value {
        Value::String(s) => clamp_str(s, head, tail).map_or_else(|| value.clone(), Value::String),
        Value::Array(items) => Value::Array(items.iter().map(|v| clamp_strings(v, head, tail)).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), clamp_strings(v, head, tail))).collect())
        }
        other => other.clone(),
    }
}

/// Spec 6.2 clamp: per-string head/tail, then a 64 KB cap on the whole payload. Returns the payload
/// and its JSON text. Over the cap the per-string limits are halved until it fits; a payload that
/// still does not fit (huge arrays, many keys) keeps its short top-level scalars plus a JSON excerpt.
pub fn clamp_payload(payload: &Map<String, Value>) -> (Value, String) {
    let original = Value::Object(payload.clone());
    let (mut head, mut tail) = (FIELD_HEAD_CHARS, FIELD_TAIL_CHARS);
    let mut out = clamp_strings(&original, head, tail);
    let mut text = out.to_string();
    while text.len() > PAYLOAD_CAP_BYTES && head > MIN_HEAD_CHARS {
        head /= 2;
        tail /= 2;
        out = clamp_strings(&original, head, tail);
        text = out.to_string();
    }
    if text.len() > PAYLOAD_CAP_BYTES {
        let mut kept: Map<String, Value> = payload
            .iter()
            .filter(|(_, v)| match v {
                Value::String(s) => s.chars().count() <= KEEP_SCALAR_CHARS,
                Value::Null | Value::Bool(_) | Value::Number(_) => true,
                _ => false,
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        kept.insert("payload_clamped".into(), Value::Bool(true));
        kept.insert("excerpt".into(), Value::String(text.chars().take(EXCERPT_CHARS).collect()));
        out = Value::Object(kept);
        text = out.to_string();
    }
    (out, text)
}

pub async fn post_events(
    State(state): State<AppState>,
    identity: Identity,
    Path(session_id): Path<Uuid>,
    Json(batch): Json<EventsBatch>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, auth::EVENTS_WRITE).await?;
    batch.validate()?;

    let n = batch.events.len();
    let (mut ids, mut seqs, mut kinds, mut payloads, mut keys, mut times, mut tokens) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::<DateTime<Utc>>::with_capacity(n),
        Vec::with_capacity(n),
    );
    for ev in &batch.events {
        let (payload, text) = clamp_payload(&ev.payload);
        ids.push(new_id());
        seqs.push(ev.seq);
        kinds.push(ev.kind);
        payloads.push(payload);
        keys.push(ev.idempotency_key.as_str());
        times.push(ev.occurred_at);
        tokens.push(estimate_tokens(&text));
    }

    let mut tx = user_tx(&state.db.app, &identity).await?;
    let session = lock_own_session(&mut tx, &identity, session_id).await?;
    let inserted: Vec<NewEvent> = sqlx::query_as(
        "INSERT INTO events (id, space_id, session_id, seq, kind, payload, idempotency_key, occurred_at, token_estimate)
         SELECT u.id, $1, $2, u.seq, u.kind, u.payload, u.key, u.at, u.tokens
           FROM unnest($3::uuid[], $4::int[], $5::text[], $6::jsonb[], $7::text[], $8::timestamptz[], $9::int[])
                AS u(id, seq, kind, payload, key, at, tokens)
         ON CONFLICT DO NOTHING
         RETURNING id, seq, kind",
    )
    .bind(session.space_id)
    .bind(session_id)
    .bind(&ids)
    .bind(&seqs)
    .bind(&kinds)
    .bind(&payloads)
    .bind(&keys)
    .bind(&times)
    .bind(&tokens)
    .fetch_all(&mut *tx)
    .await?;
    let assignment =
        windows::assign_events(&mut tx, Some(&*state.prompts), &state.settings, &session, &inserted).await?;
    sqlx::query(
        "UPDATE clients SET last_seen_at = now(),
                current_project_key = coalesce($2, current_project_key),
                current_project_at = CASE WHEN $2::text IS NULL THEN current_project_at ELSE now() END
          WHERE id = $1",
    )
    .bind(identity.client_id)
    .bind(&session.project_key)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let accepted = inserted.len() as i64;
    metrics::counter!("memsvc_events_ingested_total").increment(inserted.len() as u64);
    metrics::counter!("memsvc_events_duplicate_total").increment((n - inserted.len()) as u64);
    let out = EventsAccepted {
        accepted,
        duplicates: n as i64 - accepted,
        window_no: assignment.window_no,
        window_closed: !assignment.closed.is_empty(),
    };
    Ok((StatusCode::ACCEPTED, Json(out)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clamps_long_strings_head_and_tail() {
        let s = format!("{}{}{}", "a".repeat(FIELD_HEAD_CHARS), "b".repeat(100), "c".repeat(FIELD_TAIL_CHARS));
        let out = clamp_str(&s, FIELD_HEAD_CHARS, FIELD_TAIL_CHARS).unwrap();
        assert!(out.starts_with(&"a".repeat(FIELD_HEAD_CHARS)));
        assert!(out.ends_with(&"c".repeat(FIELD_TAIL_CHARS)));
        assert!(out.contains("…[clamped 100 chars]…"));
        assert_eq!(clamp_str("short", 10, 10), None);
        // multi-byte characters are counted as chars, never split
        let wide = "é".repeat(30);
        assert_eq!(clamp_str(&wide, 5, 5).unwrap(), format!("{}…[clamped 20 chars]…{}", "é".repeat(5), "é".repeat(5)));
    }

    #[test]
    fn payload_cap_and_excerpt() {
        let big = "x".repeat(20_000);
        let payload = json!({"tool_name": "Edit", "tool_input": {"a": big, "b": [big, big]}, "n": 3});
        let (out, text) = clamp_payload(payload.as_object().unwrap());
        assert!(text.len() <= PAYLOAD_CAP_BYTES);
        assert_eq!(out["tool_name"], "Edit");
        assert!(out["tool_input"]["a"].as_str().unwrap().contains("[clamped"));

        // too many strings to fit even after halving: excerpt fallback keeps short scalars
        let many: Vec<String> = (0..3000).map(|i| format!("{i:0>64}")).collect();
        let payload = json!({"tool_name": "Bash", "items": many, "flag": true});
        let (out, text) = clamp_payload(payload.as_object().unwrap());
        assert!(text.len() <= PAYLOAD_CAP_BYTES);
        assert_eq!(out["payload_clamped"], true);
        assert_eq!(out["tool_name"], "Bash");
        assert_eq!(out["flag"], true);
        assert!(out.get("items").is_none());
    }
}
