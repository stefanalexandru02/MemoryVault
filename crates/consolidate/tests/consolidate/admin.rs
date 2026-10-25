//! `/v1/admin/*`: scope enforcement and the operations behind the routes.

use axum::Router;
use axum::http::{Method, StatusCode};
use memvault_core::auth::scopes::TokenKind;
use memvault_core::auth::verify_token;
use memvault_core::ids::new_id;
use memvault_core::schemas::common::PacketKind;
use memvault_core::test_support::{Seeded, TEST_PEPPER, app, seed_identity, send};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::*;

/// action, target table, actor, before, after.
type RemapAudit = (String, String, String, Option<Value>, Option<Value>);

struct Api {
    c: Ctx,
    app: Router,
    admin: Seeded,
}

async fn api(pool: PgPool) -> Api {
    let c = ctx(pool.clone());
    let admin = seed_identity(&pool, TokenKind::Admin).await;
    let app = app(c.state.clone(), memvault_consolidate::router());
    Api { c, app, admin }
}

impl Api {
    async fn call(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let r = send(&self.app, method, uri, Some(&self.admin.token), body).await;
        let status = r.status;
        (status, if r.body.is_empty() { Value::Null } else { r.json() })
    }

    async fn ok(&self, method: Method, uri: &str, body: Option<Value>) -> Value {
        let (status, body) = self.call(method, uri, body).await;
        assert!(status.is_success(), "{uri}: {status} {body}");
        body
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_route_needs_the_admin_scope(pool: PgPool) {
    let a = api(pool.clone()).await;
    let device = seed_identity(&pool, TokenKind::Device).await;
    let runner = seed_identity(&pool, TokenKind::Runner).await;
    let id = Uuid::now_v7();
    let routes: Vec<(Method, String, Option<Value>)> = vec![
        (Method::POST, "/v1/admin/users".into(), Some(json!({"handle": "carol"}))),
        (Method::GET, "/v1/admin/users".into(), None),
        (Method::POST, "/v1/admin/spaces".into(), Some(json!({"name": "s", "owner_id": id}))),
        (Method::GET, "/v1/admin/spaces".into(), None),
        (Method::POST, "/v1/admin/memberships".into(), Some(json!({"space_id": id, "user_id": id, "role": "reader"}))),
        (Method::GET, "/v1/admin/memberships".into(), None),
        (Method::POST, "/v1/admin/tokens".into(), Some(json!({"handle": "x", "device_id": "d"}))),
        (Method::GET, "/v1/admin/tokens".into(), None),
        (Method::DELETE, format!("/v1/admin/tokens/{id}"), None),
        (Method::POST, "/v1/admin/consolidate".into(), None),
        (Method::GET, "/v1/admin/consolidation".into(), None),
        (Method::POST, format!("/v1/admin/consolidation/{id}/revert"), None),
        (Method::GET, "/v1/admin/jobs".into(), None),
        (Method::GET, "/v1/admin/packets?status=failed".into(), None),
        (Method::POST, format!("/v1/admin/packets/{id}/retry"), None),
        (Method::GET, "/v1/admin/doctor".into(), None),
        (Method::POST, "/v1/admin/reindex".into(), None),
        (Method::POST, "/v1/admin/project-remap".into(), Some(json!({"from": "a", "to": "b"}))),
    ];
    for (method, uri, body) in routes {
        for who in [&device, &runner] {
            let r = send(&a.app, method.clone(), &uri, Some(&who.token), body.clone()).await;
            assert_eq!(r.status, StatusCode::FORBIDDEN, "{method} {uri}: {}", r.text());
            assert_eq!(r.error_code().as_deref(), Some("insufficient_scope"), "{method} {uri}");
        }
        let r = send(&a.app, method.clone(), &uri, None, body.clone()).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        // the same request with an admin token gets past the scope check
        let r = send(&a.app, method.clone(), &uri, Some(&a.admin.token), body).await;
        assert_ne!(r.status, StatusCode::FORBIDDEN, "{method} {uri}: {}", r.text());
        assert_ne!(r.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
    }
    // only the admin's own request created the user: the refused ones (two tokens) created nothing
    assert_eq!(count(&pool, "SELECT count(*) FROM users WHERE handle = 'carol'").await, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn users_spaces_memberships(pool: PgPool) {
    let a = api(pool.clone()).await;
    let (status, alex) =
        a.call(Method::POST, "/v1/admin/users", Some(json!({"handle": "alex", "display_name": "Alex"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!((alex["handle"].as_str(), alex["display_name"].as_str()), (Some("alex"), Some("Alex")));
    assert!(alex["default_space_id"].is_string(), "a private default space is created");
    let (status, dup) = a.call(Method::POST, "/v1/admin/users", Some(json!({"handle": "alex"}))).await;
    assert_eq!((status, dup["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")));
    let (status, bad) = a.call(Method::POST, "/v1/admin/users", Some(json!({"handle": "Not Valid"}))).await;
    assert_eq!((status, bad["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_request")));
    let bob = a.ok(Method::POST, "/v1/admin/users", Some(json!({"handle": "bob"}))).await;

    let users = a.ok(Method::GET, "/v1/admin/users", None).await;
    let handles: Vec<&str> = users.as_array().unwrap().iter().map(|u| u["handle"].as_str().unwrap()).collect();
    assert!(handles.contains(&"alex") && handles.contains(&"bob"));

    let (status, team) =
        a.call(Method::POST, "/v1/admin/spaces", Some(json!({"name": "team", "owner_id": alex["id"]}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!((team["kind"].as_str(), team["owner_id"].clone()), (Some("shared"), alex["id"].clone()));
    let (status, unknown) =
        a.call(Method::POST, "/v1/admin/spaces", Some(json!({"name": "x", "owner_id": Uuid::now_v7()}))).await;
    assert_eq!((status, unknown["error"]["code"].as_str()), (StatusCode::NOT_FOUND, Some("not_found")));
    assert!(a.ok(Method::GET, "/v1/admin/spaces", None).await.as_array().unwrap().iter().any(|s| s["name"] == "team"));

    let m = json!({"space_id": team["id"], "user_id": bob["id"], "role": "reader"});
    let (status, _) = a.call(Method::POST, "/v1/admin/memberships", Some(m)).await;
    assert_eq!(status, StatusCode::CREATED);
    // promoting is the same call
    let m = json!({"space_id": team["id"], "user_id": bob["id"], "role": "writer"});
    a.ok(Method::POST, "/v1/admin/memberships", Some(m)).await;
    let list =
        a.ok(Method::GET, &format!("/v1/admin/memberships?space_id={}", team["id"].as_str().unwrap()), None).await;
    let roles: Vec<(&str, &str)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["user_id"].as_str().unwrap(), m["role"].as_str().unwrap()))
        .collect();
    assert_eq!(roles.len(), 2);
    assert!(roles.contains(&(bob["id"].as_str().unwrap(), "writer")));
    let (status, _) = a
        .call(
            Method::POST,
            "/v1/admin/memberships",
            Some(json!({"space_id": Uuid::now_v7(), "user_id": bob["id"], "role": "reader"})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations")]
async fn tokens_are_minted_once_and_revocable(pool: PgPool) {
    let a = api(pool.clone()).await;
    let alex = a.ok(Method::POST, "/v1/admin/users", Some(json!({"handle": "alex"}))).await;

    let (status, minted) = a
        .call(Method::POST, "/v1/admin/tokens", Some(json!({"handle": "alex", "device_id": "laptop-x1", "kind": "device", "hostname": "x1", "expires_days": 30})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");
    let raw = minted["token"].as_str().unwrap().to_string();
    assert!(raw.starts_with("mem_") && raw.len() == 45);
    assert_eq!(minted["kind"], "device");
    let scopes: Vec<&str> = minted["scopes"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    assert_eq!(scopes, vec!["events:write", "memories:read", "memories:write", "work:lease"]);
    let days = (chrono::DateTime::parse_from_rfc3339(minted["expires_at"].as_str().unwrap()).unwrap().to_utc()
        - chrono::Utc::now())
    .num_days();
    assert!((29..=30).contains(&days), "{days}");
    assert_eq!(minted["user_id"], alex["id"]);

    // the minted token authenticates as alex on laptop-x1 with a client created for it
    let mut conn = pool.acquire().await.unwrap();
    let identity = verify_token(&mut conn, &raw, TEST_PEPPER).await.unwrap();
    assert_eq!((identity.device_id.as_str(), identity.client_kind.as_str()), ("laptop-x1", "claude-code"));
    assert_eq!(identity.user_id.to_string(), alex["id"].as_str().unwrap());
    drop(conn);
    // but it cannot administer
    let r = send(&a.app, Method::GET, "/v1/admin/users", Some(&raw), None).await;
    assert_eq!(r.error_code().as_deref(), Some("insufficient_scope"));

    // listings never show the secret
    let list = a.ok(Method::GET, &format!("/v1/admin/tokens?user_id={}", alex["id"].as_str().unwrap()), None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(!list.to_string().contains(&raw) && list[0].get("token").is_none() && list[0].get("secret_hash").is_none());
    assert_eq!(list[0]["prefix"], minted["prefix"]);

    // a runner/admin kind, and the unknown-user / validation errors
    let (_, runner) = a
        .call(
            Method::POST,
            "/v1/admin/tokens",
            Some(json!({"user_id": alex["id"], "device_id": "nightly", "kind": "runner"})),
        )
        .await;
    assert_eq!(runner["scopes"], json!(["work:lease", "memories:read"]));
    let (status, _) =
        a.call(Method::POST, "/v1/admin/tokens", Some(json!({"handle": "ghost", "device_id": "d"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = a.call(Method::POST, "/v1/admin/tokens", Some(json!({"device_id": "d"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = a
        .call(Method::POST, "/v1/admin/tokens", Some(json!({"handle": "alex", "device_id": "d", "expires_days": 0})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // revoke by id: immediate, idempotent; unknown is a 404
    let id = minted["token_id"].as_str().unwrap();
    let revoked = a.ok(Method::DELETE, &format!("/v1/admin/tokens/{id}"), None).await;
    assert!(revoked["revoked_at"].is_string());
    let mut conn = pool.acquire().await.unwrap();
    let err = verify_token(&mut conn, &raw, TEST_PEPPER).await.unwrap_err();
    assert_eq!(err.code, "token_revoked");
    drop(conn);
    assert!(a.call(Method::DELETE, &format!("/v1/admin/tokens/{id}"), None).await.0.is_success());
    assert_eq!(
        a.call(Method::DELETE, &format!("/v1/admin/tokens/{}", Uuid::now_v7()), None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn consolidate_now_and_revert_over_http(pool: PgPool) {
    let a = api(pool.clone()).await;
    let alice = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    let lone = fact(&pool, alice.space_id, subject, FactSpec::default()).await;

    let out = a.ok(Method::POST, "/v1/admin/consolidate", Some(json!({"space_id": alice.space_id}))).await;
    assert_eq!(out["actions"]["auto_activate"], 1);
    assert_eq!(status_of(&pool, lone).await, "active");
    let run = out["run_id"].as_str().unwrap().to_string();

    let r = a.ok(Method::POST, &format!("/v1/admin/consolidation/{run}/revert"), None).await;
    assert_eq!(
        (r["run_id"].as_str(), r["reverted"].as_i64(), r["packets_cancelled"].as_i64()),
        (Some(run.as_str()), Some(1), Some(0))
    );
    assert_eq!(status_of(&pool, lone).await, "candidate");
    let actor: String =
        sqlx::query_scalar("SELECT actor FROM memory_audit WHERE action = 'revert'").fetch_one(&pool).await.unwrap();
    assert_eq!(actor, format!("client:{}", a.admin.client_id));
    assert_eq!(
        a.call(Method::POST, &format!("/v1/admin/consolidation/{}/revert", Uuid::now_v7()), None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        a.call(Method::POST, "/v1/admin/consolidation/not-a-uuid/revert", None).await.0,
        StatusCode::BAD_REQUEST
    );

    // no body = every space
    let all = a.ok(Method::POST, "/v1/admin/consolidate", None).await;
    assert_eq!(all["actions"]["auto_activate"], 1);
    assert_eq!(
        a.call(Method::POST, "/v1/admin/consolidate", Some(json!({"space_id": "nope"}))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn failed_packets_can_be_listed_and_retried(pool: PgPool) {
    let a = api(pool.clone()).await;
    let alice = seed(&pool).await;
    let failed = a.c.packet(alice.space_id, PacketKind::ConsolidateFacts, json!({"run_id": new_id()})).await;
    let pending = a.c.packet(alice.space_id, PacketKind::ConsolidateSummary, json!({"run_id": new_id()})).await;
    sqlx::query(
        "UPDATE work_packets SET status = 'failed', attempts = 3, last_error = 'quota_exhausted' WHERE id = $1",
    )
    .bind(failed.id)
    .execute(&pool)
    .await
    .unwrap();

    let list = a.ok(Method::GET, "/v1/admin/packets?status=failed", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(
        (list[0]["id"].as_str(), list[0]["status"].as_str(), list[0]["last_error"].as_str()),
        (Some(failed.id.to_string().as_str()), Some("failed"), Some("quota_exhausted"))
    );
    let all = a.ok(Method::GET, "/v1/admin/packets?kind=consolidate_summary&limit=10", None).await;
    assert_eq!(all[0]["id"].as_str(), Some(pending.id.to_string().as_str()));
    assert_eq!(a.call(Method::GET, "/v1/admin/packets?status=bogus", None).await.0, StatusCode::UNPROCESSABLE_ENTITY);

    let retried = a.ok(Method::POST, &format!("/v1/admin/packets/{}/retry", failed.id), None).await;
    assert_eq!(
        (retried["status"].as_str(), retried["attempts"].as_i64(), retried["last_error"].is_null()),
        (Some("pending"), Some(0), true)
    );
    let (status, conflict) = a.call(Method::POST, &format!("/v1/admin/packets/{}/retry", failed.id), None).await;
    assert_eq!((status, conflict["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")));
    assert_eq!(
        a.call(Method::POST, &format!("/v1/admin/packets/{}/retry", Uuid::now_v7()), None).await.0,
        StatusCode::NOT_FOUND
    );

    // keeping the attempts grants exactly one more try
    sqlx::query("UPDATE work_packets SET status = 'failed', attempts = 3 WHERE id = $1")
        .bind(failed.id)
        .execute(&pool)
        .await
        .unwrap();
    let kept = a
        .ok(Method::POST, &format!("/v1/admin/packets/{}/retry", failed.id), Some(json!({"reset_attempts": false})))
        .await;
    assert_eq!(
        (kept["attempts"].as_i64(), kept["max_attempts"].as_i64(), kept["status"].as_str()),
        (Some(3), Some(4), Some("pending"))
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn reindex_queues_embed_jobs_for_live_rows(pool: PgPool) {
    let a = api(pool.clone()).await;
    let alice = seed(&pool).await;
    let bob = seed(&pool).await;
    let subject = entity(&pool, alice.space_id, "alex").await;
    fact(&pool, alice.space_id, subject, FactSpec { status: "active", ..Default::default() }).await;
    fact(&pool, alice.space_id, subject, FactSpec { status: "retracted", predicate: "r", ..Default::default() }).await;
    observation(&pool, alice.space_id, ObsSpec::default()).await;
    observation(&pool, alice.space_id, ObsSpec::default()).await;
    observation(&pool, bob.space_id, ObsSpec::default()).await;

    let r = a
        .ok(Method::POST, "/v1/admin/reindex", Some(json!({"tables": ["observations"], "space_id": alice.space_id})))
        .await;
    assert_eq!(r["jobs_enqueued"], 2);
    let payload: Value =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE type = 'embed' LIMIT 1").fetch_one(&pool).await.unwrap();
    assert_eq!(payload["owner_table"], "observations");
    assert_eq!(payload["model"], "bge-m3");
    assert!(payload["owner_id"].is_string());

    let r = a.ok(Method::POST, "/v1/admin/reindex", Some(json!({"model": "bge-m3-v2"}))).await;
    // 3 observations + 1 live fact + 1 entity of alice, 1 entity of nothing else
    assert_eq!(r["jobs_enqueued"], 5, "{r}");
    assert_eq!(count(&pool, "SELECT count(*) FROM jobs WHERE payload->>'model' = 'bge-m3-v2'").await, 5);
    let jobs = a.ok(Method::GET, "/v1/admin/jobs?type=embed&status=queued&limit=3", None).await;
    assert_eq!(jobs.as_array().unwrap().len(), 3);
    assert_eq!(jobs[0]["type"], "embed");
    let all = a.ok(Method::GET, "/v1/admin/jobs", None).await;
    assert_eq!(all.as_array().unwrap().len(), 7);
}

#[sqlx::test(migrations = "../../migrations")]
async fn project_remap_is_audited_and_revertable(pool: PgPool) {
    let a = api(pool.clone()).await;
    let alice = seed(&pool).await;
    let bob = seed(&pool).await;
    let space = alice.space_id;
    let subject = entity(&pool, space, "alex").await;
    let sess = session(&pool, space, alice.client_id, "old-key").await;
    let obs =
        observation(&pool, space, ObsSpec { scope_key: "old-key", session_id: Some(sess), ..Default::default() }).await;
    let keep = observation(&pool, space, ObsSpec { scope_key: "unrelated", ..Default::default() }).await;
    let f =
        fact(&pool, space, subject, FactSpec { status: "active", scope_key: "old-key", ..Default::default() }).await;
    embed_fact(&a.c, f, "vim").await;
    // another space's rows with the same key are untouched when a space is given
    let b_subject = entity(&pool, bob.space_id, "bob").await;
    let bobs =
        fact(&pool, bob.space_id, b_subject, FactSpec { status: "active", scope_key: "old-key", ..Default::default() })
            .await;
    sqlx::query("INSERT INTO profiles (space_id, scope_kind, scope_key, body_md, token_estimate, facts_version) VALUES ($1, 'project', 'old-key', 'stale', 1, 0)")
        .bind(space)
        .execute(&pool)
        .await
        .unwrap();

    let r = a
        .ok(
            Method::POST,
            "/v1/admin/project-remap",
            Some(json!({"from": "old-key", "to": "new-key", "space_id": space})),
        )
        .await;
    let run = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(
        (
            r["rows"]["sessions"].as_i64(),
            r["rows"]["observations"].as_i64(),
            r["rows"]["facts"].as_i64(),
            r["rows"]["embeddings"].as_i64()
        ),
        (Some(1), Some(1), Some(1), Some(1))
    );
    assert_eq!(text(&pool, "SELECT project_key FROM sessions WHERE id = $1", sess).await, "new-key");
    assert_eq!(text(&pool, "SELECT scope_key FROM observations WHERE id = $1", obs).await, "new-key");
    assert_eq!(text(&pool, "SELECT scope_key FROM observations WHERE id = $1", keep).await, "unrelated");
    assert_eq!(text(&pool, "SELECT scope_key FROM facts WHERE id = $1", f).await, "new-key");
    assert_eq!(text(&pool, "SELECT scope_key FROM embeddings WHERE owner_id = $1", f).await, "new-key");
    assert_eq!(text(&pool, "SELECT scope_key FROM facts WHERE id = $1", bobs).await, "old-key");
    let profiles: Vec<String> = sqlx::query_scalar("SELECT scope_key FROM profiles WHERE space_id = $1")
        .bind(space)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(profiles, vec!["new-key"], "the old key's profile is dropped, the new key's rendered");

    let audit: Vec<RemapAudit> = sqlx::query_as(
        "SELECT action, target_table, actor, before, after FROM memory_audit WHERE run_id = $1::uuid ORDER BY seq",
    )
    .bind(&run)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audit.len(), 3);
    assert!(audit.iter().all(|r| r.0 == "project.remap" && r.2 == format!("client:{}", a.admin.client_id)));
    let fact_row = audit.iter().find(|r| r.1 == "facts").unwrap();
    assert_eq!(
        (fact_row.3.as_ref().unwrap()["scope_key"].as_str(), fact_row.4.as_ref().unwrap()["scope_key"].as_str()),
        (Some("old-key"), Some("new-key"))
    );

    // an active fact on the target key blocks a second remap into it
    let (status, err) =
        a.call(Method::POST, "/v1/admin/project-remap", Some(json!({"from": "other", "to": "new-key"}))).await;
    assert_eq!(status, StatusCode::OK, "{err}");
    fact(&pool, space, subject, FactSpec { status: "active", scope_key: "third", ..Default::default() }).await;
    let (status, err) =
        a.call(Method::POST, "/v1/admin/project-remap", Some(json!({"from": "third", "to": "new-key"}))).await;
    assert_eq!((status, err["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")));
    assert_eq!(
        a.call(Method::POST, "/v1/admin/project-remap", Some(json!({"from": "x", "to": "x"}))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // revert puts everything back
    let rev = a.ok(Method::POST, &format!("/v1/admin/consolidation/{run}/revert"), None).await;
    assert_eq!(rev["reverted"], 3);
    assert_eq!(text(&pool, "SELECT project_key FROM sessions WHERE id = $1", sess).await, "old-key");
    assert_eq!(text(&pool, "SELECT scope_key FROM observations WHERE id = $1", obs).await, "old-key");
    assert_eq!(text(&pool, "SELECT scope_key FROM facts WHERE id = $1", f).await, "old-key");
    assert_eq!(text(&pool, "SELECT scope_key FROM embeddings WHERE owner_id = $1", f).await, "old-key");
    let profiles: Vec<String> = sqlx::query_scalar("SELECT scope_key FROM profiles WHERE space_id = $1 ORDER BY 1")
        .bind(space)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(profiles.contains(&"old-key".to_string()) && !profiles.contains(&"new-key".to_string()), "{profiles:?}");
}
