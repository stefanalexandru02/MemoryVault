//! Planner -> lease -> result over the real routes (ingest's work routes, this crate's appliers).

use axum::http::{Method, StatusCode};
use memvault_consolidate::planner::run_planner;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::test_support::{SeedOptions, app, seed_identity_with, send};
use serde_json::json;
use sqlx::PgPool;

use crate::common::*;

#[sqlx::test(migrations = "../../migrations")]
async fn a_planned_cluster_is_leased_resolved_and_applied(pool: PgPool) {
    let c = ctx(pool.clone());
    // the runner is the space's owner here: it may lease packets of the space
    let runner = seed_identity_with(&pool, TokenKind::Runner, SeedOptions::default()).await;
    let space = runner.space_id;
    let subject = entity(&pool, space, "alex").await;
    let evidence = observation(&pool, space, ObsSpec { title: "moved to emacs", ..Default::default() }).await;
    let old = fact(&pool, space, subject, FactSpec { status: "active", value_text: "vim", ..Default::default() }).await;
    let new = fact(
        &pool,
        space,
        subject,
        FactSpec {
            value_text: "emacs",
            conflict: true,
            supersedes_id: Some(old),
            evidence: vec![evidence],
            ..Default::default()
        },
    )
    .await;
    let planned = run_planner(&c.state, Some(space)).await.unwrap();
    assert_eq!(planned.packets_created["consolidate_facts"], 1);

    let router = axum::Router::new().merge(memvault_ingest::router()).merge(memvault_consolidate::router());
    let app = app(c.state.clone(), router);
    let lease =
        send(&app, Method::POST, "/v1/work/lease", Some(&runner.token), Some(json!({"kinds": ["consolidate_facts"]})))
            .await;
    assert_eq!(lease.status, StatusCode::OK, "{}", lease.text());
    let packet = lease.json()["packets"][0].clone();
    assert_eq!(packet["kind"], "consolidate_facts");
    assert_eq!(packet["output_schema"], "consolidate.facts.v1");
    assert_eq!(packet["model_hint"], "opus");
    let user = packet["prompt"]["user"].as_str().unwrap();
    assert!(user.contains("moved to emacs") && user.contains(&new.to_string()), "{user}");
    assert!(packet["prompt"]["system"].as_str().unwrap().contains("supersede"));
    let id = packet["id"].as_str().unwrap();
    let uri = format!("/v1/work/{id}/result");

    // an action the schema does not know is a 422 with retry text; the lease is kept
    let bad = send(
        &app,
        Method::POST,
        &uri,
        Some(&runner.token),
        Some(json!({"output": {"decisions": [{"action": "delete_everything", "reason": "x"}]}})),
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", bad.text());
    assert_eq!(bad.error_code().as_deref(), Some("schema_invalid"));
    assert_eq!(status_of(&pool, new).await, "candidate");

    let output = json!({"decisions": [{"action": "supersede", "target_id": old, "by_candidate_id": new, "reason": "user switched"}]});
    let fenced = format!("```json\n{output}\n```");
    let ok = send(
        &app,
        Method::POST,
        &uri,
        Some(&runner.token),
        Some(json!({"output": fenced, "meta": {"model": "claude-opus-test", "runner": "nightly"}})),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text());
    assert_eq!(ok.json()["applied"]["supersede"], 1);
    assert_eq!(status_of(&pool, old).await, "superseded");
    assert_eq!(status_of(&pool, new).await, "active");
    let (model, prompt, run): (Option<String>, Option<String>, uuid::Uuid) = sqlx::query_as(
        "SELECT model, prompt_version, run_id FROM memory_audit WHERE target_id = $1 AND action = 'fact.supersede'",
    )
    .bind(old)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (model.as_deref(), prompt.as_deref(), run),
        (Some("claude-opus-test"), Some("consolidate.facts@v1"), planned.run_id)
    );

    // a duplicate post returns the first application's counts and changes nothing
    let again = send(&app, Method::POST, &uri, Some(&runner.token), Some(json!({"output": output}))).await;
    assert_eq!(again.json()["duplicate"], true);
    assert_eq!(again.json()["applied"]["supersede"], 1);
    assert_eq!(count(&pool, "SELECT count(*) FROM memory_audit WHERE action = 'fact.supersede'").await, 1);

    // and the run can still be reverted over HTTP by an admin
    let admin = seed_identity_with(&pool, TokenKind::Admin, SeedOptions::default()).await;
    let rev = send(
        &app,
        Method::POST,
        &format!("/v1/admin/consolidation/{}/revert", planned.run_id),
        Some(&admin.token),
        None,
    )
    .await;
    assert_eq!(rev.status, StatusCode::OK, "{}", rev.text());
    assert_eq!(rev.json()["reverted"], 2);
    assert_eq!(status_of(&pool, old).await, "active");
}
