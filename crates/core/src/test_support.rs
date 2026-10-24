//! Helpers for `#[sqlx::test(migrations = "../../migrations")]` tests in every crate
//! (feature `test-support`; add `memvault-core = { workspace = true, features = ["test-support"] }`
//! to `[dev-dependencies]`).
//!
//! ```ignore
//! #[sqlx::test(migrations = "../../migrations")]
//! async fn creates_session(pool: PgPool) {
//!     let alice = seed_identity(&pool, TokenKind::Device).await;
//!     let app = app(test_state(pool.clone()), memvault_ingest::router());
//!     let resp = send(&app, Method::POST, "/v1/sessions", Some(&alice.token), Some(json!({...}))).await;
//!     assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
//! }
//! ```
//!
//! The `#[sqlx::test]` pool connects as the superuser; `user_tx`/`system_tx` switch to the real
//! roles, so RLS and grants behave as in production.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use crate::appliers::AppliersRegistry;
use crate::auth::Identity;
use crate::auth::admin::{add_membership, create_space, create_user, ensure_client};
use crate::auth::scopes::TokenKind;
use crate::auth::tokens::{MintOptions, mint_token, verify_token};
use crate::config::Settings;
use crate::db::{Db, system_tx};
use crate::http::api_router;
use crate::prompts::PromptRegistry;
use crate::schemas::common::{MembershipRole, SpaceKind};
use crate::schemas::validate::OutputSchemas;
use crate::state::AppState;
use crate::tei::FakeTei;

pub const TEST_PEPPER: &str = "test-pepper";

/// The repository root (for `schemas/`, `prompts/`, `migrations/`).
pub fn repo_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

/// Settings for tests: FakeTei, pepper `test-pepper`, rate limits 1,000,000/min, repository
/// `schemas/` and `prompts/` directories.
pub fn test_settings() -> Settings {
    Settings {
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        fake_tei: true,
        token_pepper: TEST_PEPPER.into(),
        rate_limit_events_per_min: 1_000_000,
        rate_limit_default_per_min: 1_000_000,
        prompts_dir: repo_dir().join("prompts"),
        schemas_dir: repo_dir().join("schemas"),
        log_format: "text".into(),
        ..Settings::default()
    }
}

/// [`AppState`] over the test pool (both roles), [`test_settings`], a [`FakeTei`] and no appliers.
pub fn test_state(pool: PgPool) -> AppState {
    test_state_with(pool, test_settings(), AppliersRegistry::default()).0
}

/// Like [`test_state`] with explicit settings and appliers; also returns the [`FakeTei`] handle
/// (e.g. `tei.set_down(true)`, `tei.calls`).
pub fn test_state_with(pool: PgPool, settings: Settings, appliers: AppliersRegistry) -> (AppState, Arc<FakeTei>) {
    let tei = Arc::new(FakeTei::new(&settings.embed_model, settings.embed_dims));
    let prompts = PromptRegistry::load(&settings.prompts_dir).expect("loading prompts");
    let schemas = OutputSchemas::load(&settings.schemas_dir).expect("loading output schemas");
    let state = AppState {
        settings: Arc::new(settings),
        db: Db::single(pool),
        tei: tei.clone(),
        prompts: Arc::new(prompts),
        schemas: Arc::new(schemas),
        appliers: Arc::new(appliers),
    };
    (state, tei)
}

/// A seeded user with a private default space (owner), a client and a minted token.
#[derive(Debug, Clone)]
pub struct Seeded {
    pub user_id: Uuid,
    /// The user's private default space.
    pub space_id: Uuid,
    pub client_id: Uuid,
    pub device_id: String,
    pub handle: String,
    /// Raw bearer token.
    pub token: String,
    pub token_id: Uuid,
    pub identity: Identity,
}

impl Seeded {
    /// `Bearer <token>`.
    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

/// Options for [`seed_identity_with`].
#[derive(Debug, Clone, Default)]
pub struct SeedOptions {
    pub handle: Option<String>,
    pub device_id: Option<String>,
    /// Default: the kind's scopes.
    pub scopes: Option<Vec<String>>,
}

/// Seed a user + private space + client + token of `kind`.
pub async fn seed_identity(pool: &PgPool, kind: TokenKind) -> Seeded {
    seed_identity_with(pool, kind, SeedOptions::default()).await
}

pub async fn seed_identity_with(pool: &PgPool, kind: TokenKind, options: SeedOptions) -> Seeded {
    let suffix = Uuid::now_v7().simple().to_string();
    let suffix = &suffix[suffix.len() - 10..];
    let handle = options.handle.unwrap_or_else(|| format!("user-{suffix}"));
    let device_id = options.device_id.unwrap_or_else(|| format!("dev-{suffix}"));
    let mut tx = system_tx(pool).await.expect("system_tx");
    let user = create_user(&mut tx, &handle, None).await.expect("create_user");
    let client_id =
        ensure_client(&mut tx, user.user_id, &device_id, kind.client_kind(), None, None).await.expect("ensure_client");
    let minted = mint_token(
        &mut tx,
        user.user_id,
        client_id,
        kind,
        TEST_PEPPER,
        MintOptions { scopes: options.scopes, ..MintOptions::default() },
    )
    .await
    .expect("mint_token");
    tx.commit().await.expect("commit");
    let mut conn = pool.acquire().await.expect("acquire");
    let identity = verify_token(&mut conn, &minted.token, TEST_PEPPER).await.expect("verify_token");
    Seeded {
        user_id: user.user_id,
        space_id: user.space_id,
        client_id,
        device_id,
        handle,
        token: minted.token,
        token_id: minted.token_id,
        identity,
    }
}

/// A shared space owned by `owner`, with `members` added at their roles.
pub async fn seed_shared_space(pool: &PgPool, name: &str, owner: Uuid, members: &[(Uuid, MembershipRole)]) -> Uuid {
    let mut tx = system_tx(pool).await.expect("system_tx");
    let space = create_space(&mut tx, name, SpaceKind::Shared, owner).await.expect("create_space");
    for (user, role) in members {
        add_membership(&mut tx, space, *user, *role).await.expect("add_membership");
    }
    tx.commit().await.expect("commit");
    space
}

/// `api_router(v1).with_state(state)`: the module router nested under `/v1` with the server's
/// fallbacks and body limit, ready for [`send`].
pub fn app(state: AppState, v1: Router<AppState>) -> Router {
    api_router(v1).with_state(state)
}

/// A collected response.
#[derive(Debug, Clone)]
pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl TestResponse {
    /// The body as JSON (panics with the text when it is not JSON).
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// `body.error.code`, if this is an error response.
    pub fn error_code(&self) -> Option<String> {
        serde_json::from_slice::<Value>(&self.body).ok()?.pointer("/error/code")?.as_str().map(String::from)
    }
}

/// Send one request through `app` (no network). `token` adds `Authorization: Bearer <token>`;
/// `body` is sent as JSON.
pub async fn send(app: &Router, method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> TestResponse {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())),
        None => req.body(Body::empty()),
    }
    .expect("request");
    send_request(app, req).await
}

/// Send a fully built request through `app`.
pub async fn send_request(app: &Router, req: Request<Body>) -> TestResponse {
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    TestResponse { status, headers, body }
}
