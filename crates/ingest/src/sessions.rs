//! Spec 6.1: `POST /v1/sessions`, `POST /v1/sessions/{id}/end` (events:write).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use memvault_core::auth::{self, resolve_write_space};
use memvault_core::http::{Json, Path};
use memvault_core::ids::new_id;
use memvault_core::schemas::sessions::{SessionCreate, SessionCreated, SessionEnd, SessionEnded, WindowPolicy};
use memvault_core::{ApiError, AppState, Identity, Settings, require_scope};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::builders::{SessionRow, lock_session};
use crate::tx::user_tx;
use crate::windows;

pub fn window_policy(settings: &Settings) -> WindowPolicy {
    WindowPolicy { tool_uses: settings.window_tool_uses, idle_seconds: settings.window_idle_seconds }
}

/// Lock a session the caller's client owns: 404 when invisible (RLS), 403 when another client's.
pub async fn lock_own_session(
    conn: &mut PgConnection,
    identity: &Identity,
    session_id: Uuid,
) -> Result<SessionRow, ApiError> {
    let session = lock_session(conn, session_id).await?.ok_or_else(|| ApiError::not_found("session not found"))?;
    if session.client_id != identity.client_id {
        return Err(ApiError::forbidden("forbidden", "session belongs to another client"));
    }
    Ok(session)
}

/// Idempotent on `(client, external_session_id)`: a repeat returns 200 with the existing id.
pub async fn create_session(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<SessionCreate>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, auth::EVENTS_WRITE).await?;
    body.validate()?;
    let mut tx = user_tx(&state.db.app, &identity).await?;
    let space_id = resolve_write_space(&mut tx, &identity, body.space_id).await?;
    let inserted: Option<(Uuid, Uuid)> = sqlx::query_as(
        "INSERT INTO sessions (id, space_id, client_id, harness, external_session_id, project_key, cwd, metadata)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (client_id, external_session_id) DO NOTHING
         RETURNING id, space_id",
    )
    .bind(new_id())
    .bind(space_id)
    .bind(identity.client_id)
    .bind(&body.harness)
    .bind(&body.external_session_id)
    .bind(&body.project_key)
    .bind(&body.cwd)
    .bind(serde_json::Value::Object(body.metadata.clone()))
    .fetch_optional(&mut *tx)
    .await?;
    let (status, (session_id, space_id)) = match inserted {
        Some(row) => (StatusCode::CREATED, row),
        None => {
            let existing: Option<(Uuid, Uuid)> =
                sqlx::query_as("SELECT id, space_id FROM sessions WHERE client_id = $1 AND external_session_id = $2")
                    .bind(identity.client_id)
                    .bind(&body.external_session_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            // exists in a space this user can no longer see
            let row = existing.ok_or_else(|| ApiError::conflict("session exists but is not visible to this user"))?;
            (StatusCode::OK, row)
        }
    };
    if let Some(project_key) = &body.project_key {
        sqlx::query(
            "UPDATE clients SET current_project_key = $2, current_project_at = now(), last_seen_at = now()
              WHERE id = $1",
        )
        .bind(identity.client_id)
        .bind(project_key)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let out = SessionCreated { session_id, space_id, window_policy: window_policy(&state.settings) };
    Ok((status, Json(out)))
}

/// Close the open window (creating its extract packet) and owe `summarize_session` (9.1). The
/// body `{"reason": ...}` is optional.
pub async fn end_session(
    State(state): State<AppState>,
    identity: Identity,
    Path(session_id): Path<Uuid>,
    body: Bytes,
) -> Result<Json<SessionEnded>, ApiError> {
    require_scope(&state, &identity, auth::EVENTS_WRITE).await?;
    let body: SessionEnd = if body.iter().all(u8::is_ascii_whitespace) {
        SessionEnd::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::invalid(format!("invalid body: {e}")))?
    };
    let mut tx = user_tx(&state.db.app, &identity).await?;
    let session = lock_own_session(&mut tx, &identity, session_id).await?;
    let ended = windows::end_session(&mut tx, Some(&*state.prompts), &session, body.reason.as_deref()).await?;
    tx.commit().await?;
    Ok(Json(SessionEnded {
        session_id,
        ended_at: ended.ended_at,
        window_closed: ended.window_closed,
        packets_created: ended.packets.iter().map(|k| k.as_str().to_string()).collect(),
    }))
}
