//! `memvault admin ...`: the command line over the same operations.

use memvault_consolidate::admin_cli::run_to;
use memvault_core::auth::verify_token;
use memvault_core::schemas::common::PacketKind;
use memvault_core::test_support::TEST_PEPPER;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::common::*;

async fn admin(c: &Ctx, args: &[&str]) -> anyhow::Result<Value> {
    let mut out = Vec::new();
    run_to(c.state.clone(), args.iter().map(|a| a.to_string()).collect(), &mut out).await?;
    Ok(serde_json::from_slice(&out).expect("the command prints JSON"))
}

#[sqlx::test(migrations = "../../migrations")]
async fn tenancy_commands(pool: PgPool) {
    let c = ctx(pool.clone());
    let alex = admin(&c, &["user", "create", "alex", "--display-name", "Alex"]).await.unwrap();
    assert_eq!((alex["handle"].as_str(), alex["display_name"].as_str()), (Some("alex"), Some("Alex")));
    admin(&c, &["user", "create", "bob"]).await.unwrap();
    assert!(admin(&c, &["user", "create", "alex"]).await.unwrap_err().to_string().contains("conflict"));
    assert_eq!(admin(&c, &["user", "list"]).await.unwrap().as_array().unwrap().len(), 2);

    let team = admin(&c, &["space", "create", "team", "--owner", "alex"]).await.unwrap();
    assert_eq!(team["kind"], "shared");
    let m = admin(&c, &["membership", "add", "--space", "team", "--user", "bob", "--role", "writer"]).await.unwrap();
    assert_eq!(m["role"], "writer");
    let by_id = admin(&c, &["membership", "list", "--space", team["id"].as_str().unwrap()]).await.unwrap();
    assert_eq!(by_id.as_array().unwrap().len(), 2);
    assert_eq!(
        admin(&c, &["space", "list"]).await.unwrap().as_array().unwrap().len(),
        3,
        "two private spaces and the team"
    );
    assert!(admin(&c, &["membership", "add", "--space", "nope", "--user", "bob", "--role", "reader"]).await.is_err());
    assert!(admin(&c, &["membership", "add", "--space", "team", "--user", "bob", "--role", "king"]).await.is_err());
}

#[sqlx::test(migrations = "../../migrations")]
async fn token_create_prints_the_raw_token_once(pool: PgPool) {
    let c = ctx(pool.clone());
    admin(&c, &["user", "create", "alex"]).await.unwrap();
    let minted =
        admin(&c, &["token", "create", "--user", "alex", "--client", "laptop-x1", "--kind", "device"]).await.unwrap();
    let raw = minted["token"].as_str().unwrap().to_string();
    let mut conn = pool.acquire().await.unwrap();
    let identity = verify_token(&mut conn, &raw, TEST_PEPPER).await.unwrap();
    assert_eq!((identity.device_id.as_str(), identity.token_kind.as_str()), ("laptop-x1", "device"));
    assert!(identity.has("events:write") && !identity.has("admin"));
    drop(conn);

    let listed = admin(&c, &["token", "list", "--user", "alex"]).await.unwrap();
    assert!(!listed.to_string().contains(&raw), "listing never shows the token");
    assert_eq!(listed[0]["prefix"], minted["prefix"]);

    let runner = admin(
        &c,
        &["token", "create", "--user", "alex", "--client", "nightly", "--kind", "runner", "--expires-days", "7"],
    )
    .await
    .unwrap();
    assert_eq!(runner["scopes"], json!(["work:lease", "memories:read"]));
    let admin_tok =
        admin(&c, &["token", "create", "--user", "alex", "--client", "ops", "--kind", "admin"]).await.unwrap();
    assert!(admin_tok["scopes"].as_array().unwrap().contains(&json!("admin")));

    // revoke by prefix
    let prefix = minted["prefix"].as_str().unwrap();
    let revoked = admin(&c, &["token", "revoke", prefix]).await.unwrap();
    assert!(revoked["revoked_at"].is_string());
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(verify_token(&mut conn, &raw, TEST_PEPPER).await.unwrap_err().code, "token_revoked");

    assert!(
        admin(&c, &["token", "create", "--user", "ghost", "--client", "d"])
            .await
            .unwrap_err()
            .to_string()
            .contains("not_found")
    );
    assert!(admin(&c, &["token", "create", "--user", "alex", "--client", "d", "--kind", "agent"]).await.is_err());
    assert!(admin(&c, &["token", "revoke", "zzzzzzzz"]).await.is_err());
}

#[sqlx::test(migrations = "../../migrations")]
async fn consolidation_and_maintenance_commands(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    let lone = fact(&pool, alice.space_id, subject, FactSpec::default()).await;
    let obs = observation(&pool, alice.space_id, ObsSpec { scope_key: "old", ..Default::default() }).await;

    let planned = admin(&c, &["consolidate", "now", "--space-id", &alice.space_id.to_string()]).await.unwrap();
    assert_eq!(planned["actions"]["auto_activate"], 1);
    assert_eq!(status_of(&pool, lone).await, "active");
    let run = planned["run_id"].as_str().unwrap();
    let reverted = admin(&c, &["consolidate", "revert", run]).await.unwrap();
    assert_eq!(reverted["reverted"], 1);
    assert_eq!(status_of(&pool, lone).await, "candidate");
    let actor: String =
        sqlx::query_scalar("SELECT actor FROM memory_audit WHERE action = 'revert'").fetch_one(&pool).await.unwrap();
    assert_eq!(actor, "admin-cli");
    assert!(admin(&c, &["consolidate", "revert", "not-a-uuid"]).await.is_err());
    assert!(
        admin(&c, &["consolidate", "revert", &uuid::Uuid::now_v7().to_string()])
            .await
            .unwrap_err()
            .to_string()
            .contains("not_found")
    );

    let p = c.packet(alice.space_id, PacketKind::ConsolidateFacts, json!({"run_id": uuid::Uuid::now_v7()})).await;
    sqlx::query("UPDATE work_packets SET status = 'failed', attempts = 3 WHERE id = $1")
        .bind(p.id)
        .execute(&pool)
        .await
        .unwrap();
    let failed = admin(&c, &["packets", "list", "--status", "failed"]).await.unwrap();
    assert_eq!(failed[0]["id"].as_str(), Some(p.id.to_string().as_str()));
    let retried = admin(&c, &["packets", "retry", &p.id.to_string()]).await.unwrap();
    assert_eq!((retried["status"].as_str(), retried["attempts"].as_i64()), (Some("pending"), Some(0)));
    assert!(admin(&c, &["packets", "retry", &p.id.to_string()]).await.is_err());

    let queued =
        admin(&c, &["reindex", "--table", "observations", "--space-id", &alice.space_id.to_string()]).await.unwrap();
    assert_eq!(queued["jobs_enqueued"], 1);
    assert_eq!(admin(&c, &["jobs", "list", "--type", "embed"]).await.unwrap().as_array().unwrap().len(), 1);

    let remapped = admin(&c, &["project", "remap", "old", "new"]).await.unwrap();
    assert_eq!(remapped["rows"]["observations"], 1);
    assert_eq!(text(&pool, "SELECT scope_key FROM observations WHERE id = $1", obs).await, "new");
    let undone = admin(&c, &["consolidate", "revert", remapped["run_id"].as_str().unwrap()]).await.unwrap();
    assert_eq!(undone["reverted"], 1);
    assert_eq!(text(&pool, "SELECT scope_key FROM observations WHERE id = $1", obs).await, "old");

    assert!(admin(&c, &["frobnicate"]).await.is_err());
    assert!(admin(&c, &[]).await.is_err());
}
