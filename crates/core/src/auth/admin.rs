//! Tenancy bootstrap shared by the admin API, the admin CLI and tests. Run on a `system_tx`
//! connection: users/spaces/clients/api_tokens have no RLS, but writes to them are admin-only.

use sqlx::PgConnection;
use uuid::Uuid;

use crate::ids::new_id;
use crate::schemas::common::{ClientKind, MembershipRole, SpaceKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatedUser {
    pub user_id: Uuid,
    /// The private default space.
    pub space_id: Uuid,
}

/// Create a space and make `owner_id` its owner member.
pub async fn create_space(
    conn: &mut PgConnection,
    name: &str,
    kind: SpaceKind,
    owner_id: Uuid,
) -> Result<Uuid, sqlx::Error> {
    let space_id = new_id();
    sqlx::query("INSERT INTO spaces (id, name, kind, owner_id) VALUES ($1, $2, $3, $4)")
        .bind(space_id)
        .bind(name)
        .bind(kind)
        .bind(owner_id)
        .execute(&mut *conn)
        .await?;
    add_membership(conn, space_id, owner_id, MembershipRole::Owner).await?;
    Ok(space_id)
}

/// Insert or update a membership.
pub async fn add_membership(
    conn: &mut PgConnection,
    space_id: Uuid,
    user_id: Uuid,
    role: MembershipRole,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO memberships (space_id, user_id, role) VALUES ($1, $2, $3)
         ON CONFLICT (space_id, user_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(space_id)
    .bind(user_id)
    .bind(role)
    .execute(conn)
    .await?;
    Ok(())
}

/// Create a user with one private space set as `default_space_id` (spec D4).
pub async fn create_user(
    conn: &mut PgConnection,
    handle: &str,
    display_name: Option<&str>,
) -> Result<CreatedUser, sqlx::Error> {
    let user_id = new_id();
    sqlx::query("INSERT INTO users (id, handle, display_name) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(handle)
        .bind(display_name.unwrap_or(handle))
        .execute(&mut *conn)
        .await?;
    let space_id = create_space(conn, &format!("{handle} (private)"), SpaceKind::Private, user_id).await?;
    sqlx::query("UPDATE users SET default_space_id = $2 WHERE id = $1")
        .bind(user_id)
        .bind(space_id)
        .execute(conn)
        .await?;
    Ok(CreatedUser { user_id, space_id })
}

/// The client id for `(user, device_id)`, created when missing (hostname/platform refreshed).
pub async fn ensure_client(
    conn: &mut PgConnection,
    user_id: Uuid,
    device_id: &str,
    kind: ClientKind,
    hostname: Option<&str>,
    platform: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO clients (id, user_id, kind, device_id, hostname, platform) VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (user_id, device_id) DO UPDATE
           SET hostname = coalesce(EXCLUDED.hostname, clients.hostname),
               platform = coalesce(EXCLUDED.platform, clients.platform)
         RETURNING id",
    )
    .bind(new_id())
    .bind(user_id)
    .bind(kind)
    .bind(device_id)
    .bind(hostname)
    .bind(platform)
    .fetch_one(conn)
    .await
}
