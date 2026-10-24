//! Bearer tokens: `mem_<prefix 8>_<secret 32>`, base62 (spec 5.2).
//!
//! The server stores the prefix and `HMAC-SHA256(TOKEN_PEPPER, secret)`; the raw token is shown
//! once. Lookup is by prefix, then a constant-time hash compare, then revocation, expiry and
//! disabled-user checks. `last_used_at` / `clients.last_seen_at` are updated at most once a minute.

use chrono::{DateTime, TimeDelta, Utc};
use hmac::{Hmac, KeyInit, Mac};
use rand::RngExt;
use sha2::Sha256;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::Identity;
use crate::auth::scopes::{ALL_SCOPES, TokenKind};
use crate::error::ApiError;
use crate::ids::new_id;

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
pub const PREFIX_LEN: usize = 8;
pub const SECRET_LEN: usize = 32;

fn rand62(n: usize) -> String {
    let mut rng = rand::rng();
    (0..n).map(|_| BASE62[rng.random_range(0..BASE62.len())] as char).collect()
}

/// `HMAC-SHA256(pepper, secret)`.
pub fn hash_secret(secret: &str, pepper: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper.as_bytes()).expect("HMAC takes any key length");
    mac.update(secret.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// `(prefix, secret)`, or `None` when `raw` is not a well-formed token.
pub fn parse_token(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.trim().strip_prefix("mem_")?;
    let (prefix, secret) = rest.split_once('_')?;
    let ok = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_alphanumeric());
    (ok(prefix, PREFIX_LEN) && ok(secret, SECRET_LEN)).then_some((prefix, secret))
}

/// Options for [`mint_token`]; defaults are the kind's scopes and TTL.
#[derive(Debug, Clone, Default)]
pub struct MintOptions {
    pub scopes: Option<Vec<String>>,
    pub expires_in: Option<TimeDelta>,
    pub never_expires: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedToken {
    /// Raw bearer value; show once, never store.
    pub token: String,
    pub token_id: Uuid,
    pub prefix: String,
    pub user_id: Uuid,
    pub client_id: Uuid,
    pub kind: TokenKind,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Create an `api_tokens` row. Unknown scopes are rejected with 422.
pub async fn mint_token(
    conn: &mut PgConnection,
    user_id: Uuid,
    client_id: Uuid,
    kind: TokenKind,
    pepper: &str,
    options: MintOptions,
) -> Result<MintedToken, ApiError> {
    let scopes: Vec<String> = options.scopes.unwrap_or_else(|| kind.scopes().iter().map(|s| s.to_string()).collect());
    if let Some(bad) = scopes.iter().find(|s| !ALL_SCOPES.contains(&s.as_str())) {
        return Err(ApiError::invalid(format!("unknown scope {bad:?}")));
    }
    let expires_at = (!options.never_expires).then(|| Utc::now() + options.expires_in.unwrap_or_else(|| kind.ttl()));
    let token_id = new_id();
    for _ in 0..5 {
        // Prefix collisions are astronomically rare; ON CONFLICT keeps the transaction usable.
        let (prefix, secret) = (rand62(PREFIX_LEN), rand62(SECRET_LEN));
        let inserted = sqlx::query(
            "INSERT INTO api_tokens (id, user_id, client_id, kind, prefix, secret_hash, scopes, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (prefix) DO NOTHING",
        )
        .bind(token_id)
        .bind(user_id)
        .bind(client_id)
        .bind(kind.as_str())
        .bind(&prefix)
        .bind(hash_secret(&secret, pepper))
        .bind(&scopes)
        .bind(expires_at)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if inserted == 1 {
            return Ok(MintedToken {
                token: format!("mem_{prefix}_{secret}"),
                token_id,
                prefix,
                user_id,
                client_id,
                kind,
                scopes,
                expires_at,
            });
        }
    }
    Err(ApiError::internal("could not allocate a unique token prefix"))
}

/// Revoke immediately (verification reads the row on every request). `false` if unknown or
/// already revoked.
pub async fn revoke_token(conn: &mut PgConnection, token_id: Uuid) -> Result<bool, sqlx::Error> {
    let n = sqlx::query("UPDATE api_tokens SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
        .bind(token_id)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(n == 1)
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    id: Uuid,
    user_id: Uuid,
    client_id: Uuid,
    kind: String,
    scopes: Vec<String>,
    secret_hash: Vec<u8>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    stale: bool,
    default_space_id: Option<Uuid>,
    disabled_at: Option<DateTime<Utc>>,
    device_id: String,
    client_kind: String,
}

/// Resolve a raw bearer token to an [`Identity`], or a 401 [`ApiError`].
pub async fn verify_token(conn: &mut PgConnection, raw: &str, pepper: &str) -> Result<Identity, ApiError> {
    let (prefix, secret) =
        parse_token(raw).ok_or_else(|| ApiError::unauthorized("invalid_token", "malformed token"))?;
    let row: Option<TokenRow> = sqlx::query_as(
        "SELECT t.id, t.user_id, t.client_id, t.kind, t.scopes, t.secret_hash, t.expires_at, t.revoked_at,
                (t.last_used_at IS NULL OR t.last_used_at < now() - interval '1 minute') AS stale,
                u.default_space_id, u.disabled_at, c.device_id, c.kind AS client_kind
           FROM api_tokens t
           JOIN users u ON u.id = t.user_id
           JOIN clients c ON c.id = t.client_id
          WHERE t.prefix = $1",
    )
    .bind(prefix)
    .fetch_optional(&mut *conn)
    .await?;
    let digest = hash_secret(secret, pepper);
    let row = match row {
        Some(r) if r.secret_hash.len() == digest.len() && constant_time_eq(&r.secret_hash, &digest) => r,
        _ => return Err(ApiError::unauthorized("invalid_token", "unknown token")),
    };
    if row.revoked_at.is_some() {
        return Err(ApiError::unauthorized("token_revoked", "token has been revoked"));
    }
    if row.expires_at.is_some_and(|e| e <= Utc::now()) {
        return Err(ApiError::unauthorized("token_expired", "token has expired"));
    }
    if row.disabled_at.is_some() {
        return Err(ApiError::unauthorized("user_disabled", "user is disabled"));
    }
    if row.stale {
        sqlx::query(
            "WITH t AS (UPDATE api_tokens SET last_used_at = now() WHERE id = $1)
             UPDATE clients SET last_seen_at = now() WHERE id = $2",
        )
        .bind(row.id)
        .bind(row.client_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(Identity {
        user_id: row.user_id,
        client_id: row.client_id,
        scopes: row.scopes,
        default_space_id: row.default_space_id,
        token_id: row.id,
        token_kind: row.kind,
        device_id: row.device_id,
        client_kind: row.client_kind,
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_hash() {
        let raw = format!("mem_{}_{}", rand62(8), rand62(32));
        let (p, s) = parse_token(&raw).unwrap();
        assert_eq!((p.len(), s.len()), (8, 32));
        for bad in [
            "",
            "mem_short_x",
            "Bearer abc",
            &format!("mem_ABCDEFGH_{}", "x".repeat(31)),
            &format!("mem_ABCDEFG!_{}", "x".repeat(32)),
        ] {
            assert!(parse_token(bad).is_none(), "{bad}");
        }
        assert_eq!(hash_secret("s", "p"), hash_secret("s", "p"));
        assert_ne!(hash_secret("s", "p"), hash_secret("s", "q"));
        assert_eq!(hash_secret("s", "p").len(), 32);
    }
}
