//! Work state (spec 6.5): append-only rows folded on read, last write per key wins.
//!
//! The fold itself is `memvault_recall::engine::fold_work_state`, shared with recall and context.

use axum::extract::State;
use axum::response::IntoResponse;
use memvault_core::auth::{MEMORIES_READ, MEMORIES_WRITE, resolve_read_spaces, resolve_write_space};
use memvault_core::http::{Json, Query};
use memvault_core::ids::new_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::work_state::{WorkStateList, WorkStateQuery, WorkStateResponse, WorkStateWrite};
use memvault_core::{ApiError, AppState, Identity, require_scope};
use memvault_recall::engine::{default_scopes, fold_work_state, validate_scopes};

use crate::db::user_tx;
use crate::memories::write_scope;

pub(crate) async fn get(
    State(state): State<AppState>,
    identity: Identity,
    Query(query): Query<WorkStateQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    Ok(Json(read(&state, &identity, query).await?))
}

pub(crate) async fn post(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<WorkStateWrite>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_WRITE).await?;
    Ok(Json(write(&state, &identity, body).await?))
}

/// Folded lists of the given scope (default: global, the current project and this device),
/// optionally one list.
pub async fn read(state: &AppState, identity: &Identity, query: WorkStateQuery) -> Result<WorkStateResponse, ApiError> {
    let mut tx = user_tx(&state.db.app, identity).await?;
    let spaces = resolve_read_spaces(&mut tx, identity, query.space_ids.as_deref()).await?;
    let scopes = match &query.scope {
        Some(scope) => validate_scopes(std::slice::from_ref(scope))?,
        None => default_scopes(&mut tx, identity).await?,
    };
    let mut lists = fold_work_state(&mut tx, &spaces, &scopes).await?;
    tx.commit().await?;
    if let Some(name) = &query.list_name {
        lists.retain(|l| &l.list_name == name);
    }
    Ok(WorkStateResponse { lists })
}

/// Append one write (a `null` value removes its key) and return the folded list.
pub async fn write(state: &AppState, identity: &Identity, body: WorkStateWrite) -> Result<WorkStateResponse, ApiError> {
    body.validate()?;
    let mut tx = user_tx(&state.db.app, identity).await?;
    let space = resolve_write_space(&mut tx, identity, body.space_id).await?;
    let scope = write_scope(&mut tx, identity, body.scope.as_ref()).await?;
    sqlx::query(
        "INSERT INTO work_state (id, space_id, scope_kind, scope_key, list_name, fields, written_by_client_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(new_id())
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(&body.list_name)
    .bind(serde_json::Value::Object(body.fields))
    .bind(identity.client_id)
    .execute(&mut *tx)
    .await?;
    notify_invalidate(&mut tx, space, scope.kind, &scope.key).await?;
    let lists: Vec<WorkStateList> = fold_work_state(&mut tx, &[space], std::slice::from_ref(&scope))
        .await?
        .into_iter()
        .filter(|l| l.list_name == body.list_name)
        .collect();
    tx.commit().await?;
    Ok(WorkStateResponse { lists })
}
