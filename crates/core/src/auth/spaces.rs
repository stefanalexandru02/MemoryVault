//! Readable/writable space resolution (spec 5.3, architecture 13).
//!
//! Explicit `space_id`s in a request are validated against the caller's memberships before any
//! query: a space the user is not a member of yields 403 `space_forbidden`. Without explicit ids,
//! reads default to the user's `default_space_id` plus every shared space they belong to; writes
//! default to `default_space_id`. `memberships` is RLS-limited to the caller's own rows for
//! `memsvc_app`, so call these on a `user_tx` connection for the same user (or a `system_tx` one).

use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::Identity;
use crate::error::ApiError;

/// Adds the space(s) a request works on to the current span (`space_id`, declared by the server's
/// `http` span), so its log lines carry them.
fn record_spaces(spaces: &[Uuid]) {
    let text = spaces.iter().map(Uuid::to_string).collect::<Vec<_>>().join(",");
    tracing::Span::current().record("space_id", text.as_str());
}

pub async fn readable_space_ids(conn: &mut PgConnection, user_id: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT space_id FROM memberships WHERE user_id = $1 ORDER BY space_id")
        .bind(user_id)
        .fetch_all(conn)
        .await
}

pub async fn writable_space_ids(conn: &mut PgConnection, user_id: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT space_id FROM memberships WHERE user_id = $1 AND role IN ('owner','writer') ORDER BY space_id",
    )
    .bind(user_id)
    .fetch_all(conn)
    .await
}

/// Spaces a read covers (deduplicated, request order). 403 if any requested id is not readable.
/// Default: the default space first, then shared spaces.
pub async fn resolve_read_spaces(
    conn: &mut PgConnection,
    identity: &Identity,
    requested: Option<&[Uuid]>,
) -> Result<Vec<Uuid>, ApiError> {
    if let Some(requested) = requested.filter(|r| !r.is_empty()) {
        let readable = readable_space_ids(conn, identity.user_id).await?;
        if let Some(foreign) = requested.iter().find(|s| !readable.contains(s)) {
            return Err(ApiError::forbidden("space_forbidden", format!("not a member of space {foreign}")));
        }
        let mut out: Vec<Uuid> = Vec::with_capacity(requested.len());
        for s in requested {
            if !out.contains(s) {
                out.push(*s);
            }
        }
        record_spaces(&out);
        return Ok(out);
    }
    let spaces: Vec<Uuid> = sqlx::query_scalar(
        "SELECT m.space_id FROM memberships m JOIN spaces s ON s.id = m.space_id
          WHERE m.user_id = $1 AND (s.id = $2 OR s.kind = 'shared')
          ORDER BY (s.id = $2) DESC, s.id",
    )
    .bind(identity.user_id)
    .bind(identity.default_space_id)
    .fetch_all(conn)
    .await?;
    record_spaces(&spaces);
    Ok(spaces)
}

/// The space a write lands in (default: the user's default space). 403 unless owner/writer.
pub async fn resolve_write_space(
    conn: &mut PgConnection,
    identity: &Identity,
    requested: Option<Uuid>,
) -> Result<Uuid, ApiError> {
    let target = requested
        .or(identity.default_space_id)
        .ok_or_else(|| ApiError::forbidden("space_forbidden", "no space given and the user has no default space"))?;
    if !writable_space_ids(conn, identity.user_id).await?.contains(&target) {
        return Err(ApiError::forbidden("space_forbidden", format!("no write access to space {target}")));
    }
    record_spaces(&[target]);
    Ok(target)
}
