//! Spec 10.1: `GET /v1/prompts/{name}` (memories:read), for debugging.
//!
//! `name` may be `observer.code` (newest loaded version) or a full ref `observer.code@v3`; the
//! `version` query parameter also selects one.

use axum::extract::State;
use memvault_core::auth;
use memvault_core::http::{Json, Path, Query};
use memvault_core::schemas::prompts::PromptOut;
use memvault_core::{ApiError, AppState, Identity, require_scope};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct PromptQuery {
    #[serde(default)]
    pub version: Option<String>,
}

pub async fn get_prompt(
    State(state): State<AppState>,
    identity: Identity,
    Path(name): Path<String>,
    Query(query): Query<PromptQuery>,
) -> Result<Json<PromptOut>, ApiError> {
    require_scope(&state, &identity, auth::MEMORIES_READ).await?;
    let spec = state.prompts.get(&name, query.version.as_deref()).map_err(|e| ApiError::not_found(e.to_string()))?;
    Ok(Json(PromptOut {
        name: spec.name.clone(),
        version: spec.version.clone(),
        prompt_ref: spec.prompt_ref(),
        system: spec.system.clone(),
        user_template: spec.user_template.clone(),
        output_schema: spec.output_schema,
        model_hint: spec.model_hint.clone(),
        versions: state.prompts.versions(&spec.name).into_iter().map(String::from).collect(),
    }))
}
