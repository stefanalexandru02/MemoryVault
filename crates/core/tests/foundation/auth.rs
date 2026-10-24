use axum::Router;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::routing::{get, post};
use chrono::TimeDelta;
use memvault_core::auth::admin::add_membership;
use memvault_core::auth::tokens::{hash_secret, parse_token};
use memvault_core::auth::{
    ADMIN, EVENTS_WRITE, MEMORIES_READ, MintOptions, TokenKind, check_rate_limit, mint_token, resolve_read_spaces,
    resolve_write_space, revoke_token, verify_token,
};
use memvault_core::schemas::common::MembershipRole;
use memvault_core::test_support::{
    SeedOptions, TEST_PEPPER, app, seed_identity, seed_identity_with, seed_shared_space, send, test_settings,
    test_state, test_state_with,
};
use memvault_core::{ApiError, AppState, AppliersRegistry, Identity, require_scope, system_tx, user_tx};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

async fn verify(pool: &PgPool, raw: &str) -> Result<Identity, ApiError> {
    let mut conn = pool.acquire().await.unwrap();
    verify_token(&mut conn, raw, TEST_PEPPER).await
}

#[sqlx::test(migrations = "../../migrations")]
async fn mint_and_verify(pool: PgPool) {
    let s = seed_identity(&pool, TokenKind::Device).await;
    let (prefix, secret) = parse_token(&s.token).unwrap();
    let id = verify(&pool, &s.token).await.unwrap();
    assert_eq!((id.user_id, id.client_id, id.default_space_id), (s.user_id, s.client_id, Some(s.space_id)));
    assert_eq!(id.scopes, vec!["events:write", "memories:read", "memories:write", "work:lease"]);
    assert_eq!(
        (id.device_id.as_str(), id.token_kind.as_str(), id.client_kind.as_str()),
        (s.device_id.as_str(), "device", "claude-code")
    );
    assert_eq!(id.actor(), format!("client:{}", s.client_id));
    type Ts = Option<chrono::DateTime<chrono::Utc>>;
    let (db_prefix, hash, used, expires): (String, Vec<u8>, Ts, Ts) =
        sqlx::query_as("SELECT prefix, secret_hash, last_used_at, expires_at FROM api_tokens WHERE id = $1")
            .bind(s.token_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(db_prefix, prefix);
    assert_eq!(hash, hash_secret(secret, TEST_PEPPER));
    assert!(used.is_some());
    let days = (expires.unwrap() - chrono::Utc::now()).num_days();
    assert!((364..=365).contains(&days));
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE secret_hash::text LIKE '%' || $1 || '%'")
            .bind(secret)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, 0, "raw secret never stored");
}

#[sqlx::test(migrations = "../../migrations")]
async fn kind_scopes(pool: PgPool) {
    let runner = seed_identity(&pool, TokenKind::Runner).await;
    assert_eq!(runner.identity.scopes, vec!["work:lease", "memories:read"]);
    assert_eq!(runner.identity.client_kind, "runner");
    let admin = seed_identity(&pool, TokenKind::Admin).await;
    assert_eq!(admin.identity.scopes, vec!["admin", "events:write", "memories:read", "memories:write", "work:lease"]);
    let custom = seed_identity_with(
        &pool,
        TokenKind::Device,
        SeedOptions { scopes: Some(vec![MEMORIES_READ.into()]), ..SeedOptions::default() },
    )
    .await;
    assert_eq!(custom.identity.scopes, vec!["memories:read"]);
    let mut tx = system_tx(&pool).await.unwrap();
    let err = mint_token(
        &mut tx,
        admin.user_id,
        admin.client_id,
        TokenKind::Device,
        TEST_PEPPER,
        MintOptions { scopes: Some(vec!["root".into()]), ..MintOptions::default() },
    )
    .await
    .unwrap_err();
    assert_eq!((err.status, err.code.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn malformed_tokens(pool: PgPool) {
    let long31 = format!("mem_ABCDEFGH_{}", "x".repeat(31));
    let bang = format!("mem_ABCDEFG!_{}", "x".repeat(32));
    for raw in ["", "mem_short_x", "Bearer abc", &long31, &bang] {
        let e = verify(&pool, raw).await.unwrap_err();
        assert_eq!((e.status, e.code.as_str()), (StatusCode::UNAUTHORIZED, "invalid_token"), "{raw}");
    }
    // well-formed but unknown prefix
    let e = verify(&pool, &format!("mem_ABCDEFGH_{}", "x".repeat(32))).await.unwrap_err();
    assert_eq!(e.code, "invalid_token");
}

#[sqlx::test(migrations = "../../migrations")]
async fn wrong_secret_and_wrong_pepper(pool: PgPool) {
    let s = seed_identity(&pool, TokenKind::Device).await;
    let last = s.token.chars().last().unwrap();
    let tampered = format!("{}{}", &s.token[..s.token.len() - 1], if last == 'A' { 'B' } else { 'A' });
    assert_eq!(verify(&pool, &tampered).await.unwrap_err().code, "invalid_token");
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(verify_token(&mut conn, &s.token, "other-pepper").await.unwrap_err().code, "invalid_token");
}

#[sqlx::test(migrations = "../../migrations")]
async fn revoke(pool: PgPool) {
    let s = seed_identity(&pool, TokenKind::Device).await;
    let mut tx = system_tx(&pool).await.unwrap();
    assert!(revoke_token(&mut tx, s.token_id).await.unwrap());
    assert!(!revoke_token(&mut tx, s.token_id).await.unwrap());
    tx.commit().await.unwrap();
    assert_eq!(verify(&pool, &s.token).await.unwrap_err().code, "token_revoked");
}

#[sqlx::test(migrations = "../../migrations")]
async fn expiry(pool: PgPool) {
    let s = seed_identity(&pool, TokenKind::Device).await;
    let mut tx = system_tx(&pool).await.unwrap();
    let expired = mint_token(
        &mut tx,
        s.user_id,
        s.client_id,
        TokenKind::Device,
        TEST_PEPPER,
        MintOptions { expires_in: Some(TimeDelta::seconds(-1)), ..MintOptions::default() },
    )
    .await
    .unwrap();
    let forever = mint_token(
        &mut tx,
        s.user_id,
        s.client_id,
        TokenKind::Device,
        TEST_PEPPER,
        MintOptions { never_expires: true, ..MintOptions::default() },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(verify(&pool, &expired.token).await.unwrap_err().code, "token_expired");
    assert!(forever.expires_at.is_none());
    assert!(verify(&pool, &forever.token).await.is_ok());
}

#[sqlx::test(migrations = "../../migrations")]
async fn disabled_user(pool: PgPool) {
    let s = seed_identity(&pool, TokenKind::Device).await;
    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1").bind(s.user_id).execute(&pool).await.unwrap();
    assert_eq!(verify(&pool, &s.token).await.unwrap_err().code, "user_disabled");
}

/// Test routes gated like real handlers: authenticate (Identity extractor), then require_scope.
fn scoped_router() -> Router<AppState> {
    async fn gate(state: &AppState, id: &Identity, scope: &str) -> Result<axum::Json<serde_json::Value>, ApiError> {
        require_scope(state, id, scope).await?;
        Ok(axum::Json(serde_json::json!({"user_id": id.user_id})))
    }
    Router::new()
        .route(
            "/_t/read",
            get(|State(s): State<AppState>, id: Identity| async move { gate(&s, &id, MEMORIES_READ).await }),
        )
        .route(
            "/_t/events",
            post(|State(s): State<AppState>, id: Identity| async move { gate(&s, &id, EVENTS_WRITE).await }),
        )
        .route("/_t/admin", get(|State(s): State<AppState>, id: Identity| async move { gate(&s, &id, ADMIN).await }))
}

#[sqlx::test(migrations = "../../migrations")]
async fn scope_enforcement(pool: PgPool) {
    let router = app(test_state(pool.clone()), scoped_router());
    let device = seed_identity(&pool, TokenKind::Device).await;
    let runner = seed_identity(&pool, TokenKind::Runner).await;
    let admin = seed_identity(&pool, TokenKind::Admin).await;

    let r = send(&router, Method::GET, "/v1/_t/read", None, None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.error_code().as_deref(), Some("unauthorized"));
    assert_eq!(r.headers["www-authenticate"], "Bearer");
    let r = send(&router, Method::GET, "/v1/_t/read", Some("mem_nottoken"), None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        r.json(),
        serde_json::json!({"error": {"code": "invalid_token", "message": "malformed token", "retryable": false}})
    );

    let r = send(&router, Method::GET, "/v1/_t/read", Some(&device.token), None).await;
    assert_eq!(r.json(), serde_json::json!({"user_id": device.user_id}));
    assert_eq!(send(&router, Method::POST, "/v1/_t/events", Some(&device.token), None).await.status, StatusCode::OK);
    let r = send(&router, Method::GET, "/v1/_t/admin", Some(&device.token), None).await;
    assert_eq!((r.status, r.error_code().as_deref()), (StatusCode::FORBIDDEN, Some("insufficient_scope")));

    assert_eq!(send(&router, Method::GET, "/v1/_t/read", Some(&runner.token), None).await.status, StatusCode::OK);
    assert_eq!(
        send(&router, Method::POST, "/v1/_t/events", Some(&runner.token), None).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&router, Method::GET, "/v1/_t/admin", Some(&runner.token), None).await.status,
        StatusCode::FORBIDDEN
    );

    for (method, path) in [(Method::GET, "/v1/_t/read"), (Method::POST, "/v1/_t/events"), (Method::GET, "/v1/_t/admin")]
    {
        assert_eq!(send(&router, method, path, Some(&admin.token), None).await.status, StatusCode::OK);
    }

    // revocation is immediate through the router
    let mut tx = system_tx(&pool).await.unwrap();
    revoke_token(&mut tx, device.token_id).await.unwrap();
    tx.commit().await.unwrap();
    let r = send(&router, Method::GET, "/v1/_t/read", Some(&device.token), None).await;
    assert_eq!(r.error_code().as_deref(), Some("token_revoked"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn rate_limit(pool: PgPool) {
    let mut settings = test_settings();
    settings.rate_limit_default_per_min = 3;
    settings.rate_limit_events_per_min = 5;
    let (state, _) = test_state_with(pool.clone(), settings, AppliersRegistry::default());
    let router = app(state, scoped_router());
    let s = seed_identity(&pool, TokenKind::Device).await;
    for _ in 0..3 {
        assert_eq!(send(&router, Method::GET, "/v1/_t/read", Some(&s.token), None).await.status, StatusCode::OK);
    }
    let r = send(&router, Method::GET, "/v1/_t/read", Some(&s.token), None).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.error_code().as_deref(), Some("rate_limited"));
    assert_eq!(r.json()["error"]["retryable"], true);
    let retry: i64 = r.headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry));
    // separate bucket for events:write
    assert_eq!(send(&router, Method::POST, "/v1/_t/events", Some(&s.token), None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn rate_limit_fails_open() {
    let broken = PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .unwrap();
    check_rate_limit(&broken, "token:x:default", 1).await.unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn space_resolution(pool: PgPool) {
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let bob = seed_identity(&pool, TokenKind::Device).await;
    let family = seed_shared_space(&pool, "family", alice.user_id, &[(bob.user_id, MembershipRole::Reader)]).await;
    let other = seed_shared_space(&pool, "other", alice.user_id, &[]).await;

    let mut tx = user_tx(&pool, &bob.identity).await.unwrap();
    assert_eq!(resolve_read_spaces(&mut tx, &bob.identity, None).await.unwrap(), vec![bob.space_id, family]);
    assert_eq!(resolve_read_spaces(&mut tx, &bob.identity, Some(&[family, family])).await.unwrap(), vec![family]);
    let e = resolve_read_spaces(&mut tx, &bob.identity, Some(&[alice.space_id])).await.unwrap_err();
    assert_eq!((e.status, e.code.as_str()), (StatusCode::FORBIDDEN, "space_forbidden"));
    assert!(resolve_read_spaces(&mut tx, &bob.identity, Some(&[other])).await.is_err());
    assert_eq!(resolve_write_space(&mut tx, &bob.identity, None).await.unwrap(), bob.space_id);
    let e = resolve_write_space(&mut tx, &bob.identity, Some(family)).await.unwrap_err(); // reader only
    assert_eq!(e.status, StatusCode::FORBIDDEN);
    drop(tx);

    let mut tx = user_tx(&pool, &alice.identity).await.unwrap();
    assert_eq!(resolve_write_space(&mut tx, &alice.identity, Some(family)).await.unwrap(), family);
    drop(tx);

    // promoting bob to writer opens writes
    let mut tx = system_tx(&pool).await.unwrap();
    add_membership(&mut tx, family, bob.user_id, MembershipRole::Writer).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = user_tx(&pool, &bob.identity).await.unwrap();
    assert_eq!(resolve_write_space(&mut tx, &bob.identity, Some(family)).await.unwrap(), family);
}

#[sqlx::test(migrations = "../../migrations")]
async fn identity_extractor_caches_per_request(pool: PgPool) {
    // Two extractions in one handler authenticate once (the second reads the extension).
    let router = app(
        test_state(pool.clone()),
        Router::new().route(
            "/_t/twice",
            get(|a: Identity, b: Identity| async move { axum::Json(serde_json::json!({"same": a == b})) }),
        ),
    );
    let s = seed_identity(&pool, TokenKind::Device).await;
    let r = send(&router, Method::GET, "/v1/_t/twice", Some(&s.token), None).await;
    assert_eq!(r.json(), serde_json::json!({"same": true}));
}
