//! `GET /v1/admin/doctor` and `memvault admin doctor`: the checks against seeded states.

use axum::http::{Method, StatusCode};
use memvault_consolidate::admin_cli::run_to;
use memvault_consolidate::doctor::doctor;
use memvault_core::AppliersRegistry;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::schemas::admin::{DoctorCheck, DoctorStatus};
use memvault_core::schemas::common::PacketKind;
use memvault_core::test_support::{app, seed_identity, send, test_settings, test_state_with};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::common::*;

async fn checks(c: &Ctx) -> Vec<DoctorCheck> {
    doctor(&c.state).await.checks
}

fn named<'a>(checks: &'a [DoctorCheck], name: &str) -> &'a DoctorCheck {
    checks.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no check {name:?} in {checks:?}"))
}

fn status(checks: &[DoctorCheck], name: &str) -> DoctorStatus {
    named(checks, name).status
}

#[sqlx::test(migrations = "../../migrations")]
async fn empty_database_has_no_failures(pool: PgPool) {
    let c = ctx(pool);
    let checks = checks(&c).await;
    let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "database",
            "pgvector",
            "pg_search",
            "embedding backlog",
            "embed queue",
            "embedder",
            "work packet queue",
            "runners",
            "last consolidation"
        ]
    );
    assert!(checks.iter().all(|c| c.status != DoctorStatus::Fail), "{checks:?}");
    assert_eq!(status(&checks, "database"), DoctorStatus::Ok);
    assert!(named(&checks, "database").detail.contains("migrations applied"));
    assert_eq!(status(&checks, "pgvector"), DoctorStatus::Ok);
    assert_eq!(status(&checks, "embedding backlog"), DoctorStatus::Ok);
    assert_eq!(status(&checks, "work packet queue"), DoctorStatus::Ok);
    // the test settings use the fake embedder, and nothing was ever consolidated
    let fake = named(&checks, "embedder");
    assert_eq!(fake.status, DoctorStatus::Warn);
    assert!(fake.detail.contains("FAKE_TEI") && fake.fix.is_some());
    assert_eq!(status(&checks, "last consolidation"), DoctorStatus::Warn);
    assert!(named(&checks, "database").fix.is_none());
}

#[sqlx::test(migrations = "../../migrations")]
async fn missing_migrations_fail_the_database_check(pool: PgPool) {
    let c = ctx(pool.clone());
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = (SELECT max(version) FROM _sqlx_migrations)")
        .execute(&pool)
        .await
        .unwrap();
    let checks = checks(&c).await;
    let db = named(&checks, "database");
    assert_eq!(db.status, DoctorStatus::Fail);
    assert!(db.fix.as_deref().unwrap().contains("memvault migrate"), "{db:?}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn embedding_backlog_follows_the_queue(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let obs = observation(&pool, alice.space_id, ObsSpec::default()).await;

    let checks = checks(&c).await;
    let backlog = named(&checks, "embedding backlog");
    assert_eq!(backlog.status, DoctorStatus::Warn);
    assert!(backlog.detail.contains("1 row(s) without"), "{backlog:?}");
    assert!(backlog.fix.as_deref().unwrap().contains("admin reindex"));

    // a queued job explains the gap
    sqlx::query("INSERT INTO jobs (id, type, payload) VALUES ($1, 'embed', $2)")
        .bind(uuid::Uuid::now_v7())
        .bind(json!({"owner_table": "observations", "owner_id": obs}))
        .execute(&pool)
        .await
        .unwrap();
    let checks = self::checks(&c).await;
    assert_eq!(status(&checks, "embedding backlog"), DoctorStatus::Ok);
    assert_eq!(status(&checks, "embed queue"), DoctorStatus::Ok);

    // ... until it has waited too long
    sqlx::query("UPDATE jobs SET run_after = now() - interval '10 minutes'").execute(&pool).await.unwrap();
    assert_eq!(status(&self::checks(&c).await, "embed queue"), DoctorStatus::Warn);
    sqlx::query("UPDATE jobs SET run_after = now() - interval '2 hours'").execute(&pool).await.unwrap();
    let checks = self::checks(&c).await;
    let queue = named(&checks, "embed queue");
    assert_eq!(queue.status, DoctorStatus::Fail);
    assert!(queue.fix.as_deref().unwrap().contains("memvault worker"));

    // archived observations and embedded rows are not counted
    sqlx::query("UPDATE observations SET archived_at = now() WHERE id = $1").bind(obs).execute(&pool).await.unwrap();
    let checks = self::checks(&c).await;
    assert_eq!(status(&checks, "embedding backlog"), DoctorStatus::Ok);
    assert_eq!(named(&checks, "embedding backlog").detail, "every row has a bge-m3 embedding");
}

#[sqlx::test(migrations = "../../migrations")]
async fn embedder_reports_the_service(pool: PgPool) {
    let mut settings = test_settings();
    settings.fake_tei = false;
    let (state, tei) = test_state_with(pool, settings, AppliersRegistry::default());
    let up = doctor(&state).await.checks;
    assert_eq!(status(&up, "embedder"), DoctorStatus::Ok);
    tei.set_down(true);
    let down = doctor(&state).await.checks;
    let embedder = named(&down, "embedder");
    assert_eq!(embedder.status, DoctorStatus::Fail);
    assert!(embedder.fix.as_deref().unwrap().contains("TEI_EMBED_URL"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn stalled_packets_point_at_the_runner(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let packet = c.packet(alice.space_id, PacketKind::ConsolidateFacts, json!({"run_id": uuid::Uuid::now_v7()})).await;

    let fresh = checks(&c).await;
    assert_eq!(status(&fresh, "work packet queue"), DoctorStatus::Ok);
    assert_eq!(named(&fresh, "runners").status, DoctorStatus::Warn, "a waiting packet and no runner yet");
    assert_eq!(status(&fresh, "last consolidation"), DoctorStatus::Ok);

    sqlx::query("UPDATE work_packets SET available_after = now() - interval '2 hours'").execute(&pool).await.unwrap();
    assert_eq!(status(&checks(&c).await, "work packet queue"), DoctorStatus::Warn);
    sqlx::query("UPDATE work_packets SET available_after = now() - interval '3 days'").execute(&pool).await.unwrap();
    let stalled = checks(&c).await;
    let queue = named(&stalled, "work packet queue");
    assert_eq!(queue.status, DoctorStatus::Fail);
    assert!(queue.detail.contains("1 pending (oldest 3d)"), "{queue:?}");
    assert!(queue.fix.as_deref().unwrap().contains("mem-hook observe --all"));

    // a lease a minute ago means a runner is alive
    sqlx::query(
        "UPDATE work_packets SET status = 'leased', leased_by = $2, lease_seconds = 600, lease_until = now() + interval '9 minutes' WHERE id = $1",
    )
    .bind(packet.id)
    .bind(alice.client_id)
    .execute(&pool)
    .await
    .unwrap();
    let leased = checks(&c).await;
    assert_eq!(status(&leased, "runners"), DoctorStatus::Ok);
    assert!(named(&leased, "runners").detail.starts_with("last packet leased"), "{:?}", named(&leased, "runners"));
    assert_eq!(status(&leased, "work packet queue"), DoctorStatus::Ok);
}

#[sqlx::test(migrations = "../../migrations")]
async fn old_consolidation_is_reported(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    c.packet(alice.space_id, PacketKind::ConsolidateSummary, json!({"run_id": uuid::Uuid::now_v7()})).await;
    sqlx::query("UPDATE work_packets SET created_at = now() - interval '5 days'").execute(&pool).await.unwrap();
    let checks = checks(&c).await;
    let last = named(&checks, "last consolidation");
    assert_eq!(last.status, DoctorStatus::Warn);
    assert!(last.detail.contains("5d"), "{last:?}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn route_is_admin_only_and_lists_the_checks(pool: PgPool) {
    let c = ctx(pool.clone());
    let admin = seed_identity(&pool, TokenKind::Admin).await;
    let device = seed_identity(&pool, TokenKind::Device).await;
    let app = app(c.state.clone(), memvault_consolidate::router());

    let denied = send(&app, Method::GET, "/v1/admin/doctor", Some(&device.token), None).await;
    assert_eq!((denied.status, denied.error_code().as_deref()), (StatusCode::FORBIDDEN, Some("insufficient_scope")));

    let ok = send(&app, Method::GET, "/v1/admin/doctor", Some(&admin.token), None).await;
    assert_eq!(ok.status, StatusCode::OK);
    let body: Value = ok.json();
    let first = &body["checks"][0];
    assert_eq!((first["name"].as_str(), first["status"].as_str()), (Some("database"), Some("ok")));
    assert!(first["fix"].is_null());
    assert!(body["checks"].as_array().unwrap().iter().any(|c| c["status"] == "warn" && c["fix"].is_string()));
}

#[sqlx::test(migrations = "../../migrations")]
async fn cli_prints_lines_or_json_and_fails_on_a_failed_check(pool: PgPool) {
    let c = ctx(pool.clone());
    let mut out = Vec::new();
    run_to(c.state.clone(), vec!["doctor".into()], &mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("ok   database"), "{text}");
    assert!(text.contains("warn embedder") && text.contains("     fix: "), "{text}");

    let mut out = Vec::new();
    run_to(c.state.clone(), vec!["doctor".into(), "--json".into()], &mut out).await.unwrap();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["checks"][0]["name"], "database");

    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = (SELECT max(version) FROM _sqlx_migrations)")
        .execute(&pool)
        .await
        .unwrap();
    let mut out = Vec::new();
    let err = run_to(c.state.clone(), vec!["doctor".into()], &mut out).await.unwrap_err();
    assert!(err.to_string().contains("check failed"), "{err}");
    assert!(String::from_utf8(out).unwrap().contains("fail database"));
}
