use std::time::Duration;

use memvault_core::auth::TokenKind;
use memvault_core::db::{caps, tune_vector_search};
use memvault_core::ids::new_id;
use memvault_core::invalidate::{CHANNEL, notify_invalidate, parse_payload};
use memvault_core::schemas::common::ScopeKind;
use memvault_core::test_support::seed_identity;
use memvault_core::{system_tx, user_tx};
use sqlx::PgPool;
use sqlx::postgres::{PgListener, PgPoolOptions};
use uuid::Uuid;

#[sqlx::test(migrations = "../../migrations")]
async fn user_tx_sets_role_and_settings(pool: PgPool) {
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let mut tx = user_tx(&pool, &alice.identity).await.unwrap();
    let (user, uid, cid): (String, Option<Uuid>, Option<Uuid>) =
        sqlx::query_as("SELECT current_user::text, app_user_id(), app_client_id()").fetch_one(&mut *tx).await.unwrap();
    assert_eq!((user.as_str(), uid, cid), ("memsvc_app", Some(alice.user_id), Some(alice.client_id)));
    tx.commit().await.unwrap();
    // SET LOCAL does not leak past the transaction.
    let (user, uid): (String, Option<Uuid>) =
        sqlx::query_as("SELECT current_user::text, app_user_id()").fetch_one(&pool).await.unwrap();
    assert_eq!((user.as_str(), uid), ("postgres", None));
}

#[sqlx::test(migrations = "../../migrations")]
async fn system_tx_role(pool: PgPool) {
    let mut tx = system_tx(&pool).await.unwrap();
    let user: String = sqlx::query_scalar("SELECT current_user::text").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(user, "memsvc_system");
}

/// Password for logging in as memsvc_app: `MEMSVC_APP_PASSWORD` when set (the compose stack), else a
/// test password, which is only ever set on a role that has none (a fresh CI cluster; migration 0002
/// creates the role without one). Trust-auth clusters ignore it.
async fn app_role_password(pool: &PgPool) -> String {
    if let Ok(p) = std::env::var("MEMSVC_APP_PASSWORD") {
        return p;
    }
    let unset: bool = sqlx::query_scalar("SELECT rolpassword IS NULL FROM pg_authid WHERE rolname = 'memsvc_app'")
        .fetch_one(pool)
        .await
        .unwrap();
    if unset {
        sqlx::query("ALTER ROLE memsvc_app PASSWORD 'memsvc-app-test'").execute(pool).await.unwrap();
    }
    "memsvc-app-test".to_string()
}

/// Production path: the app pool logs in as memsvc_app itself (no SET ROLE), and a system pool
/// without BYPASSRLS is refused instead of silently seeing nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn app_role_login(pool: PgPool) {
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let password = app_role_password(&pool).await;
    let opts = (*pool.connect_options()).clone().username("memsvc_app").password(&password);
    let app = PgPoolOptions::new().max_connections(2).connect_with(opts).await.unwrap();
    let c = caps(&app).await.unwrap();
    assert!(!c.superuser && !c.bypass_rls && c.current_user == "memsvc_app");
    let mut tx = user_tx(&app, &alice.identity).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(n, 1);
    drop(tx);
    let err = system_tx(&app).await.unwrap_err();
    assert!(err.to_string().contains("BYPASSRLS"), "{err}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn caps_and_vector_tuning(pool: PgPool) {
    let c = caps(&pool).await.unwrap();
    assert!(c.superuser && c.pgvector_version.is_some());
    assert_eq!(c.hnsw_iterative_scan(), c.pgvector_version.unwrap() >= (0, 8, 0));
    let mut tx = pool.begin().await.unwrap();
    tune_vector_search(&mut tx, &c, 80).await.unwrap();
    let ef: String = sqlx::query_scalar("SHOW hnsw.ef_search").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(ef, "80");
}

#[sqlx::test(migrations = "../../migrations")]
async fn vector_and_json_roundtrip(pool: PgPool) {
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let oid = new_id();
    let vec: Vec<f32> = (0..1024).map(|i| i as f32 / 1024.0).collect();
    let mut tx = user_tx(&pool, &alice.identity).await.unwrap();
    sqlx::query(
        "INSERT INTO embeddings (space_id, owner_table, owner_id, part, model, embedding, scope_kind, scope_key)
         VALUES ($1, 'observations', $2, 'title', 'm', $3, $4, '')",
    )
    .bind(alice.space_id)
    .bind(oid)
    .bind(pgvector::Vector::from(vec.clone()))
    .bind(ScopeKind::Global)
    .execute(&mut *tx)
    .await
    .unwrap();
    let got: pgvector::Vector = sqlx::query_scalar("SELECT embedding FROM embeddings WHERE owner_id = $1")
        .bind(oid)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(got.as_slice().len(), 1024);
    assert!((got.as_slice()[512] - 0.5).abs() < 1e-6);
    let meta: serde_json::Value = sqlx::query_scalar("SELECT $1::jsonb")
        .bind(serde_json::json!({"id": oid, "nested": [1, {"a": null}]}))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(meta, serde_json::json!({"id": oid.to_string(), "nested": [1, {"a": null}]}));
    // scope_kind[] binds from Vec<ScopeKind>
    let kinds: Vec<ScopeKind> = sqlx::query_scalar("SELECT unnest($1::scope_kind[])")
        .bind(vec![ScopeKind::Project, ScopeKind::Global])
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(kinds, vec![ScopeKind::Project, ScopeKind::Global]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn notify_invalidate_delivered_on_commit(pool: PgPool) {
    let mut listener = PgListener::connect_with(&pool).await.unwrap();
    listener.listen(CHANNEL).await.unwrap();
    let space = new_id();
    let mut tx = system_tx(&pool).await.unwrap();
    notify_invalidate(&mut tx, space, ScopeKind::Project, "github.com/a/b:c").await.unwrap();
    let early = tokio::time::timeout(Duration::from_millis(100), listener.recv()).await;
    assert!(early.is_err(), "delivered before commit");
    tx.commit().await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(2), listener.recv()).await.unwrap().unwrap();
    assert_eq!(parse_payload(n.payload()), Some((space, ScopeKind::Project, "github.com/a/b:c".to_string())));
}

#[sqlx::test(migrations = "../../migrations")]
async fn enqueue_embed_job(pool: PgPool) {
    let id = new_id();
    let mut tx = system_tx(&pool).await.unwrap();
    memvault_core::jobs::enqueue_embed(&mut tx, "observations", id).await.unwrap();
    tx.commit().await.unwrap();
    let (ty, payload, status): (String, serde_json::Value, String) =
        sqlx::query_as("SELECT type, payload, status FROM jobs").fetch_one(&pool).await.unwrap();
    assert_eq!((ty.as_str(), status.as_str()), ("embed", "queued"));
    assert_eq!(payload, serde_json::json!({"owner_table": "observations", "owner_id": id}));
}

struct CountingApplier;

#[async_trait::async_trait]
impl memvault_core::PacketApplier for CountingApplier {
    async fn apply(
        &self,
        conn: &mut sqlx::PgConnection,
        packet: &memvault_core::schemas::work::PacketRecord,
        output: &memvault_core::schemas::outputs::LlmOutput,
    ) -> Result<memvault_core::AppliedCounts, memvault_core::ApiError> {
        let one: i64 = sqlx::query_scalar("SELECT 1::bigint").fetch_one(conn).await?;
        let memvault_core::schemas::outputs::LlmOutput::Summary(s) = output else {
            return Err(memvault_core::ApiError::invalid("expected summary.v1"));
        };
        Ok([("summaries".to_string(), one), (format!("{}:{}", packet.kind, s.next_steps), 1)].into())
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn applier_registry(pool: PgPool) {
    use memvault_core::schemas::common::{OutputSchemaName, PacketKind};
    use memvault_core::schemas::work::PacketRecord;
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let mut tx = system_tx(&pool).await.unwrap();
    let id = new_id();
    sqlx::query(
        "INSERT INTO work_packets (id, space_id, kind, prompt_ref, model_hint, inputs, output_schema, idempotency_key)
         VALUES ($1, $2, 'summarize_prompt', 'summary.code@v1', 'sonnet', '{}', 'summary.v1', 'k1')",
    )
    .bind(id)
    .bind(alice.space_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    let packet: PacketRecord =
        sqlx::query_as("SELECT * FROM work_packets WHERE id = $1").bind(id).fetch_one(&mut *tx).await.unwrap();
    assert_eq!((packet.kind, packet.output_schema), (PacketKind::SummarizePrompt, OutputSchemaName::SummaryV1));

    let state = memvault_core::test_support::test_state(pool.clone());
    let output = state.schemas.validate(packet.output_schema, &serde_json::json!({"next_steps": "ship"})).unwrap();
    let mut registry = memvault_core::AppliersRegistry::default();
    let err = registry.apply(&mut tx, &packet, &output).await.unwrap_err();
    assert_eq!(err.code, "internal");
    registry.register(PacketKind::SummarizePrompt, CountingApplier);
    assert_eq!(registry.kinds(), vec![PacketKind::SummarizePrompt]);
    let counts = registry.apply(&mut tx, &packet, &output).await.unwrap();
    assert_eq!(counts.get("summaries"), Some(&1));
    assert_eq!(counts.get("summarize_prompt:ship"), Some(&1));
}
