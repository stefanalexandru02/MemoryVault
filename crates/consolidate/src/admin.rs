//! `/v1/admin/*` (spec 6.7). Every route needs the `admin` scope (403 `insufficient_scope`
//! otherwise) and runs on the system role: admin acts across spaces by definition.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use memvault_core::auth::{self, Identity, require_scope};
use memvault_core::http::{Json, Path, Query};
use memvault_core::schemas::admin::{
    ConsolidateRequest, ConsolidateResponse, DoctorResponse, JobOut, MembershipCreate, MembershipOut,
    PacketRetryRequest, ReindexRequest, ReindexResponse, RevertResponse, SpaceCreate, SpaceOut, TokenCreate,
    TokenMinted, TokenOut, UserCreate, UserOut,
};
use memvault_core::schemas::common::{PacketKind, PacketStatus};
use memvault_core::schemas::work::PacketRecord;
use memvault_core::{ApiError, AppState};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::ops::{self, RemapRequest, RemapResponse};
use crate::planner::run_planner;
use crate::revert::revert_run;
use memvault_core::system_tx;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/users", post(create_user).get(list_users))
        .route("/admin/spaces", post(create_space).get(list_spaces))
        .route("/admin/memberships", post(add_membership).get(list_memberships))
        .route("/admin/tokens", post(mint_token).get(list_tokens))
        .route("/admin/tokens/{id}", delete(revoke_token))
        .route("/admin/consolidate", post(consolidate))
        .route("/admin/consolidation/{run_id}/revert", post(revert))
        .route("/admin/jobs", get(list_jobs))
        .route("/admin/packets", get(list_packets))
        .route("/admin/packets/{id}/retry", post(retry_packet))
        .route("/admin/doctor", get(doctor))
        .route("/admin/reindex", post(reindex))
        .route("/admin/project-remap", post(remap_project))
}

/// A JSON body that may be left out entirely (`curl -X POST .../consolidate`).
fn optional_body<T: DeserializeOwned>(bytes: &Bytes) -> Result<Option<T>, ApiError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(bytes).map(Some).map_err(|e| ApiError::invalid(format!("invalid JSON body: {e}")))
}

async fn admin(state: &AppState, identity: &Identity) -> Result<(), ApiError> {
    require_scope(state, identity, auth::ADMIN).await
}

async fn create_user(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<UserCreate>,
) -> Result<(StatusCode, Json<UserOut>), ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    let out = ops::create_user(&mut tx, &body).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

async fn list_users(State(state): State<AppState>, identity: Identity) -> Result<Json<Vec<UserOut>>, ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    Ok(Json(ops::list_users(&mut tx).await?))
}

async fn create_space(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<SpaceCreate>,
) -> Result<(StatusCode, Json<SpaceOut>), ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    let out = ops::create_space(&mut tx, &body).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

async fn list_spaces(State(state): State<AppState>, identity: Identity) -> Result<Json<Vec<SpaceOut>>, ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    Ok(Json(ops::list_spaces(&mut tx).await?))
}

async fn add_membership(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<MembershipCreate>,
) -> Result<(StatusCode, Json<MembershipOut>), ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    let out = ops::add_membership(&mut tx, &body).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Debug, Default, Deserialize)]
struct MembershipQuery {
    #[serde(default)]
    space_id: Option<Uuid>,
}

async fn list_memberships(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<MembershipQuery>,
) -> Result<Json<Vec<MembershipOut>>, ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    Ok(Json(ops::list_memberships(&mut tx, q.space_id).await?))
}

/// 201 with the raw token, shown exactly once.
async fn mint_token(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<TokenCreate>,
) -> Result<(StatusCode, Json<TokenMinted>), ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    let out = ops::mint_token(&mut tx, &state.settings.token_pepper, &body).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Debug, Default, Deserialize)]
struct TokenQuery {
    #[serde(default)]
    user_id: Option<Uuid>,
}

async fn list_tokens(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<TokenQuery>,
) -> Result<Json<Vec<TokenOut>>, ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    Ok(Json(ops::list_tokens(&mut tx, q.user_id).await?))
}

async fn revoke_token(
    State(state): State<AppState>,
    identity: Identity,
    Path(id): Path<String>,
) -> Result<Json<TokenOut>, ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    let out = ops::revoke_token(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// Plan now; an empty body means every space.
async fn consolidate(
    State(state): State<AppState>,
    identity: Identity,
    body: Bytes,
) -> Result<Json<ConsolidateResponse>, ApiError> {
    admin(&state, &identity).await?;
    let space_id = optional_body::<ConsolidateRequest>(&body)?.and_then(|b| b.space_id);
    Ok(Json(run_planner(&state, space_id).await?))
}

async fn revert(
    State(state): State<AppState>,
    identity: Identity,
    Path(run_id): Path<Uuid>,
) -> Result<Json<RevertResponse>, ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    let out = revert_run(&mut tx, run_id, &identity.actor()).await?;
    tx.commit().await?;
    Ok(Json(out))
}

#[derive(Debug, Default, Deserialize)]
struct JobsQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "type")]
    job_type: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn list_jobs(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<JobsQuery>,
) -> Result<Json<Vec<JobOut>>, ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    Ok(Json(ops::list_jobs(&mut tx, q.status.as_deref(), q.job_type.as_deref(), q.limit).await?))
}

#[derive(Debug, Default, Deserialize)]
struct PacketsQuery {
    #[serde(default, deserialize_with = "memvault_core::schemas::common::opt_from_str::deserialize")]
    status: Option<PacketStatus>,
    #[serde(default, deserialize_with = "memvault_core::schemas::common::opt_from_str::deserialize")]
    kind: Option<PacketKind>,
    #[serde(default)]
    space_id: Option<Uuid>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn list_packets(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<PacketsQuery>,
) -> Result<Json<Vec<PacketRecord>>, ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    Ok(Json(ops::list_packets(&mut tx, q.status, q.kind, q.space_id, q.limit).await?))
}

async fn retry_packet(
    State(state): State<AppState>,
    identity: Identity,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> Result<Json<PacketRecord>, ApiError> {
    admin(&state, &identity).await?;
    let reset = optional_body::<PacketRetryRequest>(&body)?.is_none_or(|b| b.reset_attempts);
    let mut tx = system_tx(&state.db.system).await?;
    let out = ops::retry_packet(&mut tx, id, reset).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn doctor(State(state): State<AppState>, identity: Identity) -> Result<Json<DoctorResponse>, ApiError> {
    admin(&state, &identity).await?;
    Ok(Json(crate::doctor::doctor(&state).await))
}

async fn reindex(
    State(state): State<AppState>,
    identity: Identity,
    body: Bytes,
) -> Result<Json<ReindexResponse>, ApiError> {
    admin(&state, &identity).await?;
    let req = optional_body::<ReindexRequest>(&body)?.unwrap_or_default();
    let mut tx = system_tx(&state.db.system).await?;
    let out = ops::reindex(&mut tx, &req, &state.settings.embed_model).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn remap_project(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<RemapRequest>,
) -> Result<Json<RemapResponse>, ApiError> {
    admin(&state, &identity).await?;
    let mut tx = system_tx(&state.db.system).await?;
    let out = ops::remap_project(&mut tx, &body, &identity.actor()).await?;
    tx.commit().await?;
    Ok(Json(out))
}
