//! Triggered consolidation: runs restricted to one scope, the trigger every run records, the nightly
//! pass leaving recently planned scopes alone, and the `plan` jobs.

use axum::http::Method;
use chrono::{Duration, SubsecRound, Utc};
use memvault_consolidate::admin_cli::run_to;
use memvault_consolidate::planner::{JobOutcome, PlanOptions, run_plan_job, run_planner, run_planner_with};
use memvault_core::auth::scopes::TokenKind;
use memvault_core::jobs::enqueue_plan;
use memvault_core::schemas::common::{PlanTrigger, Scope, ScopeKind};
use memvault_core::test_support::{app, seed_identity, send};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::*;

fn project(key: &str) -> Scope {
    Scope::new(ScopeKind::Project, key)
}

/// A lone candidate old enough to be activated. The predicate doubles as the value, so the fake
/// embeddings of two such facts never link them into a cluster.
async fn lone_fact(
    pool: &PgPool,
    space: Uuid,
    subject: Uuid,
    scope_key: &'static str,
    predicate: &'static str,
) -> Uuid {
    let spec = FactSpec { scope_key, predicate, value_text: predicate, ..Default::default() };
    fact(pool, space, subject, spec).await
}

/// `(trigger, scope)` of the logged runs, oldest first.
async fn planner_runs(pool: &PgPool) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT trigger, CASE WHEN scope_kind IS NOT NULL THEN scope_kind::text || ':' || scope_key END
           FROM planner_runs ORDER BY created_at, run_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn planned_scopes(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT scope_kind::text || ':' || scope_key FROM scope_planning ORDER BY 1")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn nightly(space: Uuid) -> PlanOptions {
    PlanOptions::nightly(Some(space))
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_run_records_its_trigger(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    lone_fact(&pool, alice.space_id, subject, "acme", "editor").await;

    let manual = run_planner(&c.state, Some(alice.space_id)).await.unwrap();
    assert_eq!(manual.trigger, Some(PlanTrigger::Manual));
    assert_eq!(manual.actions["auto_activate"], 1);

    lone_fact(&pool, alice.space_id, subject, "other", "shell").await;
    let night = run_planner_with(&c.state, &nightly(alice.space_id), Utc::now()).await.unwrap();
    assert_eq!(night.trigger, Some(PlanTrigger::Nightly));
    assert_eq!(night.actions["auto_activate"], 1, "the scope the manual run never saw is planned");

    let (trigger, packets, actions): (String, Value, Value) =
        sqlx::query_as("SELECT trigger, packets, actions FROM planner_runs WHERE run_id = $1")
            .bind(manual.run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((trigger.as_str(), packets), ("manual", json!({})));
    assert_eq!(actions["auto_activate"], 1);
    assert_eq!(planner_runs(&pool).await, [("manual".to_string(), None), ("nightly".to_string(), None)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_scoped_run_touches_only_its_scope(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    let inside = lone_fact(&pool, alice.space_id, subject, "acme", "editor").await;
    let outside = lone_fact(&pool, alice.space_id, subject, "other", "shell").await;
    let old = Utc::now() - Duration::days(10);
    let obs_in =
        observation(&pool, alice.space_id, ObsSpec { scope_key: "acme", created_at: old, ..Default::default() }).await;
    let obs_out =
        observation(&pool, alice.space_id, ObsSpec { scope_key: "other", created_at: old, ..Default::default() }).await;

    let options =
        PlanOptions { space_id: Some(alice.space_id), scope: Some(project("acme")), trigger: PlanTrigger::Manual };
    run_planner_with(&c.state, &options, Utc::now()).await.unwrap();

    assert_eq!(status_of(&pool, inside).await, "active");
    assert_eq!(status_of(&pool, outside).await, "candidate");
    let decayed = "SELECT decayed_at IS NOT NULL FROM observations WHERE id = $1";
    assert!(sqlx::query_scalar::<_, bool>(decayed).bind(obs_in).fetch_one(&pool).await.unwrap());
    assert!(!sqlx::query_scalar::<_, bool>(decayed).bind(obs_out).fetch_one(&pool).await.unwrap());
    assert_eq!(planner_runs(&pool).await, [("manual".to_string(), Some("project:acme".to_string()))]);
    assert_eq!(planned_scopes(&pool).await, ["project:acme"]);

    let bare = PlanOptions { space_id: None, scope: Some(project("acme")), trigger: PlanTrigger::Manual };
    assert!(run_planner_with(&c.state, &bare, Utc::now()).await.is_err(), "a scope needs a space");
}

#[sqlx::test(migrations = "../../migrations")]
async fn nightly_leaves_a_scope_planned_moments_ago_alone(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    let t0 = Utc::now().trunc_subsecs(3);
    let scoped =
        PlanOptions { space_id: Some(alice.space_id), scope: Some(project("acme")), trigger: PlanTrigger::Threshold };
    run_planner_with(&c.state, &scoped, t0).await.unwrap();

    let later_in_scope = lone_fact(&pool, alice.space_id, subject, "acme", "editor").await;
    let other_scope = lone_fact(&pool, alice.space_id, subject, "other", "shell").await;

    let out = run_planner_with(&c.state, &nightly(alice.space_id), t0 + Duration::minutes(5)).await.unwrap();
    assert_eq!(out.actions["auto_activate"], 1);
    assert_eq!(status_of(&pool, other_scope).await, "active");
    assert_eq!(status_of(&pool, later_in_scope).await, "candidate", "planned five minutes ago, so skipped");

    run_planner_with(&c.state, &nightly(alice.space_id), t0 + Duration::minutes(16)).await.unwrap();
    assert_eq!(status_of(&pool, later_in_scope).await, "active", "the interval is over");
    assert_eq!(planned_scopes(&pool).await, ["project:acme", "project:other"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_manual_run_covers_scopes_planned_moments_ago(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    let scoped =
        PlanOptions { space_id: Some(alice.space_id), scope: Some(project("acme")), trigger: PlanTrigger::SessionEnd };
    run_planner_with(&c.state, &scoped, Utc::now()).await.unwrap();
    let fresh = lone_fact(&pool, alice.space_id, subject, "acme", "editor").await;

    run_planner(&c.state, Some(alice.space_id)).await.unwrap();
    assert_eq!(status_of(&pool, fresh).await, "active");
}

#[sqlx::test(migrations = "../../migrations")]
async fn plan_jobs_wait_out_the_minimum_interval(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    let first = lone_fact(&pool, alice.space_id, subject, "acme", "editor").await;
    let mut conn = pool.acquire().await.unwrap();
    assert!(enqueue_plan(&mut conn, alice.space_id, &project("acme"), PlanTrigger::SessionEnd).await.unwrap());
    assert!(!enqueue_plan(&mut conn, alice.space_id, &project("acme"), PlanTrigger::Threshold).await.unwrap());
    assert!(enqueue_plan(&mut conn, alice.space_id, &project("other"), PlanTrigger::Threshold).await.unwrap());
    let payload: Value = sqlx::query_scalar("SELECT payload FROM jobs WHERE payload #>> '{scope,key}' = 'acme'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(payload["trigger"], "session_end", "the first trigger keeps the job");

    let now = Utc::now().trunc_subsecs(3);
    assert!(matches!(run_plan_job(&c.state, &payload, now).await.unwrap(), JobOutcome::Planned));
    assert_eq!(status_of(&pool, first).await, "active");
    assert_eq!(planner_runs(&pool).await, [("session_end".to_string(), Some("project:acme".to_string()))]);

    let second = lone_fact(&pool, alice.space_id, subject, "acme", "shell").await;
    match run_plan_job(&c.state, &payload, now + Duration::minutes(1)).await.unwrap() {
        JobOutcome::Deferred(due) => assert_eq!(due, now + Duration::minutes(15)),
        JobOutcome::Planned => panic!("planned twice inside the interval"),
    }
    assert_eq!(status_of(&pool, second).await, "candidate");
    assert_eq!(planner_runs(&pool).await.len(), 1);

    assert!(matches!(
        run_plan_job(&c.state, &payload, now + Duration::minutes(16)).await.unwrap(),
        JobOutcome::Planned
    ));
    assert_eq!(status_of(&pool, second).await, "active");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_plan_job_never_plans_another_spaces_scope(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let bob = seed(&pool).await;
    let (sa, sb) = (entity(&pool, alice.space_id, "alex").await, entity(&pool, bob.space_id, "bob").await);
    let mine = lone_fact(&pool, alice.space_id, sa, "acme", "editor").await;
    let theirs = lone_fact(&pool, bob.space_id, sb, "acme", "editor").await;

    let mut conn = pool.acquire().await.unwrap();
    enqueue_plan(&mut conn, alice.space_id, &project("acme"), PlanTrigger::SessionEnd).await.unwrap();
    let payload: Value =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE type = 'plan'").fetch_one(&pool).await.unwrap();
    run_plan_job(&c.state, &payload, Utc::now()).await.unwrap();

    assert_eq!(status_of(&pool, mine).await, "active");
    assert_eq!(status_of(&pool, theirs).await, "candidate");
    let spaces: Vec<Uuid> = sqlx::query_scalar("SELECT space_id FROM planner_runs").fetch_all(&pool).await.unwrap();
    assert_eq!(spaces, [alice.space_id]);
    let spaces: Vec<Uuid> = sqlx::query_scalar("SELECT space_id FROM scope_planning").fetch_all(&pool).await.unwrap();
    assert_eq!(spaces, [alice.space_id]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_scope_can_be_queued_again_once_its_job_is_done(pool: PgPool) {
    let alice = seed(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    assert!(enqueue_plan(&mut conn, alice.space_id, &project("acme"), PlanTrigger::SessionEnd).await.unwrap());
    sqlx::query("UPDATE jobs SET status = 'running'").execute(&pool).await.unwrap();
    assert!(!enqueue_plan(&mut conn, alice.space_id, &project("acme"), PlanTrigger::SessionEnd).await.unwrap());
    sqlx::query("UPDATE jobs SET status = 'done'").execute(&pool).await.unwrap();
    assert!(enqueue_plan(&mut conn, alice.space_id, &project("acme"), PlanTrigger::SessionEnd).await.unwrap());
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_admin_api_and_cli_show_the_trigger(pool: PgPool) {
    let c = ctx(pool.clone());
    let router = app(c.state.clone(), memvault_consolidate::router());
    let admin = seed_identity(&pool, TokenKind::Admin).await;
    let alice = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    lone_fact(&pool, alice.space_id, subject, "acme", "editor").await;
    let get = |uri: String| {
        let (router, token) = (router.clone(), admin.token.clone());
        async move { send(&router, Method::GET, &uri, Some(&token), None).await.json() }
    };

    let body = json!({"space_id": alice.space_id, "scope": {"kind": "project", "key": "acme"}});
    let r = send(&router, Method::POST, "/v1/admin/consolidate", Some(&admin.token), Some(body)).await;
    assert_eq!(r.json()["trigger"], "manual", "{}", r.text());
    let r = send(
        &router,
        Method::POST,
        "/v1/admin/consolidate",
        Some(&admin.token),
        Some(json!({"scope": {"kind": "global"}})),
    )
    .await;
    assert_eq!(r.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);

    let mut conn = pool.acquire().await.unwrap();
    enqueue_plan(&mut conn, alice.space_id, &project("other"), PlanTrigger::Threshold).await.unwrap();
    enqueue_plan(&mut conn, alice.space_id, &project("acme"), PlanTrigger::SessionEnd).await.unwrap();

    let runs = get("/v1/admin/consolidation".into()).await;
    assert_eq!((runs[0]["trigger"].as_str(), runs[0]["scope"].as_str()), (Some("manual"), Some("project:acme")));
    assert_eq!(runs[0]["actions"]["auto_activate"], 1);
    assert_eq!(get("/v1/admin/consolidation?trigger=nightly".into()).await, json!([]));
    assert_eq!(
        get(format!("/v1/admin/consolidation?trigger=manual&space_id={}", alice.space_id))
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let jobs = get("/v1/admin/jobs?type=plan".into()).await;
    let mut seen: Vec<(String, String)> = jobs
        .as_array()
        .unwrap()
        .iter()
        .map(|j| (j["trigger"].as_str().unwrap().to_string(), j["scope"].as_str().unwrap().to_string()))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            ("session_end".to_string(), "project:acme".to_string()),
            ("threshold".to_string(), "project:other".to_string())
        ]
    );

    let mut out = Vec::new();
    let args = ["consolidate", "runs", "--trigger", "manual"].map(String::from).to_vec();
    run_to(c.state.clone(), args, &mut out).await.unwrap();
    let listed: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(listed[0]["trigger"], "manual");
    let mut out = Vec::new();
    let args = ["jobs", "list", "--type", "plan"].map(String::from).to_vec();
    run_to(c.state.clone(), args, &mut out).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&out).unwrap().as_array().unwrap().len(), 2);
    let args = ["consolidate", "now", "--scope", "project:acme"].map(String::from).to_vec();
    assert!(run_to(c.state.clone(), args, &mut Vec::new()).await.is_err(), "--scope needs --space-id");
}
