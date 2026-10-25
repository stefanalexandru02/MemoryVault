//! The caller's server-side defaults (`GET`/`PUT /v1/me/settings`), stored in `user_settings`.

use memvault_core::schemas::common::InjectionMode;
use memvault_core::schemas::recall::UserSettings;
use memvault_core::{ApiError, AppState, Identity, user_tx};
use sqlx::PgConnection;

/// The user's default injection mode; [`InjectionMode::Full`] while they have set none.
pub async fn injection_mode(conn: &mut PgConnection) -> Result<InjectionMode, sqlx::Error> {
    let mode: Option<InjectionMode> =
        sqlx::query_scalar("SELECT injection_mode FROM user_settings WHERE user_id = app_user_id()")
            .fetch_optional(conn)
            .await?;
    Ok(mode.unwrap_or_default())
}

pub async fn get(state: &AppState, identity: &Identity) -> Result<UserSettings, ApiError> {
    let mut tx = user_tx(&state.db.app, identity).await?;
    Ok(UserSettings { injection_mode: injection_mode(&mut tx).await? })
}

pub async fn put(state: &AppState, identity: &Identity, settings: UserSettings) -> Result<UserSettings, ApiError> {
    let mut tx = user_tx(&state.db.app, identity).await?;
    sqlx::query(
        "INSERT INTO user_settings (user_id, injection_mode) VALUES (app_user_id(), $1)
         ON CONFLICT (user_id) DO UPDATE SET injection_mode = EXCLUDED.injection_mode, updated_at = now()",
    )
    .bind(settings.injection_mode)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(settings)
}
