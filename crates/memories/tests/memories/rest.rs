//! `POST /v1/memories` and `DELETE /v1/memories/{id}`.

use axum::http::StatusCode;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::schemas::common::MembershipRole;
use memvault_core::test_support::{SeedOptions, seed_identity, seed_identity_with, seed_shared_space, send};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use uuid::Uuid;

use crate::common::{PROJECT, assert_status, fact_body, ok, uuid_of, world};

#[sqlx::test(migrations = "../../migrations")]
async fn fact_lands_active_with_provenance_job_and_audit(pool: PgPool) {
    let w = world(pool).await;
    let resp = w.post(&w.alice.token, "/memories", fact_body("prefers.editor", "neovim")).await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let body = resp.json();
    assert_eq!(body["table"], "facts");
    assert_eq!(body["status"], "active");
    assert!(body["rendered"].as_str().unwrap().contains("prefers.editor: neovim"), "{body}");
    let id = uuid_of(&body, "/id");

    let (status, confidence, client, extracted, scope_kind, space): (String, f32, Option<Uuid>, Value, String, Uuid) =
        sqlx::query_as(
            "SELECT status, confidence, written_by_client_id, extracted_by, scope_kind::text, space_id FROM facts WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(status, "active");
    assert!((confidence - 0.95).abs() < 1e-6);
    assert_eq!(client, Some(w.alice.client_id));
    assert_eq!(extracted, json!({"runner_kind": "explicit"}));
    assert_eq!(scope_kind, "global");
    assert_eq!(space, w.alice.space_id);

    assert_eq!(
        w.count("SELECT count(*) FROM jobs WHERE type = 'embed' AND payload->>'owner_table' = 'facts' AND payload->>'owner_id' = $1::text", id).await,
        1
    );
    let (action, actor): (String, String) =
        sqlx::query_as("SELECT action, actor FROM memory_audit WHERE target_id = $1")
            .bind(id)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!(action, "fact.create");
    assert_eq!(actor, format!("client:{}", w.alice.client_id));
}

#[sqlx::test(migrations = "../../migrations")]
async fn restating_a_fact_confirms_and_a_new_value_supersedes(pool: PgPool) {
    let w = world(pool).await;
    let first = uuid_of(&ok(&w.post(&w.alice.token, "/memories", fact_body("prefers.editor", "neovim")).await), "/id");
    let again = uuid_of(&ok(&w.post(&w.alice.token, "/memories", fact_body("prefers.editor", "neovim")).await), "/id");
    assert_eq!(first, again);
    let confirmations: i32 = sqlx::query_scalar("SELECT confirmations FROM facts WHERE id = $1")
        .bind(first)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(confirmations, 2);

    let second = uuid_of(&ok(&w.post(&w.alice.token, "/memories", fact_body("prefers.editor", "helix")).await), "/id");
    assert_ne!(first, second);
    let (old_status, superseded_by): (String, Option<Uuid>) =
        sqlx::query_as("SELECT status, superseded_by FROM facts WHERE id = $1")
            .bind(first)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!((old_status.as_str(), superseded_by), ("superseded", Some(second)));
    let (new_status, supersedes): (String, Option<Uuid>) =
        sqlx::query_as("SELECT status, supersedes_id FROM facts WHERE id = $1")
            .bind(second)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!((new_status.as_str(), supersedes), ("active", Some(first)));
    let active: i64 =
        sqlx::query_scalar("SELECT count(*) FROM facts WHERE status = 'active'").fetch_one(&w.pool).await.unwrap();
    assert_eq!(active, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_first_writes_of_a_fact_do_not_fail(pool: PgPool) {
    let w = world(pool).await;
    let writes = (0..6).map(|i| {
        let value = if i % 2 == 0 { "neovim" } else { "helix" };
        w.post(&w.alice.token, "/memories", fact_body("prefers.editor", value))
    });
    for resp in futures::future::join_all(writes).await {
        assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    }
    let active: i64 =
        sqlx::query_scalar("SELECT count(*) FROM facts WHERE status = 'active'").fetch_one(&w.pool).await.unwrap();
    assert_eq!(active, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn observation_is_explicit_and_searchable(pool: PgPool) {
    let w = world(pool).await;
    w.set_project(&w.alice, PROJECT).await;
    let resp = w
        .post(
            &w.alice.token,
            "/memories",
            json!({"kind": "observation", "type": "decision", "title": "Use sqlx runtime queries",
                   "narrative": "Compile-time macros need a live database during builds.",
                   "facts": ["no query! macro"], "files": ["Cargo.toml"]}),
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let id = uuid_of(&resp.json(), "/id");
    assert_eq!(resp.json()["status"], "created");

    let (extracted, client, scope_kind, scope_key, tokens): (Value, Option<Uuid>, String, String, i32) = sqlx::query_as(
        "SELECT extracted_by, written_by_client_id, scope_kind::text, scope_key, token_estimate FROM observations WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&w.pool)
    .await
    .unwrap();
    assert_eq!(extracted, json!({"runner_kind": "explicit"}));
    assert_eq!(client, Some(w.alice.client_id));
    // No scope given: the client's current project.
    assert_eq!((scope_kind.as_str(), scope_key.as_str()), ("project", PROJECT));
    assert!(tokens > 0);
    assert_eq!(
        w.count("SELECT count(*) FROM jobs WHERE type = 'embed' AND payload->>'owner_id' = $1::text", id).await,
        1
    );

    let found = ok(&w.get(&w.alice.token, "/search?q=runtime+queries").await);
    assert_eq!(found["rows"][0]["id"], id.to_string(), "{found}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn procedure_revision_retires_the_old_version(pool: PgPool) {
    let w = world(pool).await;
    let body = |text: &str| {
        json!({"kind": "procedure", "name": "Deploy homelab", "task_class": "deploy",
               "trigger_text": "deploying the homelab", "body_md": text, "scope": {"kind": "project", "key": PROJECT}})
    };
    let v1 = uuid_of(&ok(&w.post(&w.alice.token, "/memories", body("run make deploy")).await), "/id");
    let v2 = uuid_of(&ok(&w.post(&w.alice.token, "/memories", body("run make deploy ENV=prod")).await), "/id");
    let rows: Vec<(Uuid, i32, String, Option<Uuid>)> =
        sqlx::query_as("SELECT id, version, status, supersedes_id FROM procedures ORDER BY version")
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![(v1, 1, "retired".into(), None), (v2, 2, "active".into(), Some(v1))]);
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM memory_audit ORDER BY created_at, id").fetch_all(&w.pool).await.unwrap();
    assert_eq!(actions, ["proc.create", "proc.retire", "proc.revise"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn remember_signals_the_context_cache(pool: PgPool) {
    let w = world(pool).await;
    let mut listener = PgListener::connect_with(&w.pool).await.unwrap();
    listener.listen("memory_invalidate").await.unwrap();
    ok(&w.post(&w.alice.token, "/memories", fact_body("timezone", "Europe/Bucharest")).await);
    let note =
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv()).await.expect("notified").unwrap();
    assert_eq!(note.payload(), format!("{}:global:", w.alice.space_id));
}

#[sqlx::test(migrations = "../../migrations")]
async fn rejects_bad_requests_and_missing_rights(pool: PgPool) {
    let w = world(pool).await;
    // Validation.
    let bad = w.post(&w.alice.token, "/memories", fact_body("Not A Predicate", "x")).await;
    assert_status(&bad, StatusCode::UNPROCESSABLE_ENTITY, "invalid_request");
    let bad_scope = w
        .post(
            &w.alice.token,
            "/memories",
            json!({"kind": "observation", "title": "t", "scope": {"kind": "project", "key": ""}}),
        )
        .await;
    assert_status(&bad_scope, StatusCode::UNPROCESSABLE_ENTITY, "invalid_request");
    // Authentication and scopes.
    let anon = send(&w.api, "POST".parse().unwrap(), "/v1/memories", None, Some(fact_body("a", "b"))).await;
    assert_status(&anon, StatusCode::UNAUTHORIZED, "unauthorized");
    let runner = seed_identity(&w.pool, TokenKind::Runner).await;
    let denied = w.post(&runner.token, "/memories", fact_body("a", "b")).await;
    assert_status(&denied, StatusCode::FORBIDDEN, "insufficient_scope");
    // Writing into a space the caller cannot write to.
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let shared = seed_shared_space(&w.pool, "team", w.alice.user_id, &[(bob.user_id, MembershipRole::Reader)]).await;
    let mut body = fact_body("team.rule", "tests first");
    body["space_id"] = json!(shared);
    assert_status(&w.post(&bob.token, "/memories", body.clone()).await, StatusCode::FORBIDDEN, "space_forbidden");
    ok(&w.post(&w.alice.token, "/memories", body).await);
}

#[sqlx::test(migrations = "../../migrations")]
async fn forget_is_soft_audited_and_idempotent(pool: PgPool) {
    let w = world(pool).await;
    let fact = uuid_of(&ok(&w.post(&w.alice.token, "/memories", fact_body("prefers.editor", "neovim")).await), "/id");
    let obs = uuid_of(
        &ok(&w.post(&w.alice.token, "/memories", json!({"kind": "observation", "title": "Quirk of the router"})).await),
        "/id",
    );
    let proc = uuid_of(
        &ok(&w
            .post(
                &w.alice.token,
                "/memories",
                json!({"kind": "procedure", "name": "p", "task_class": "c", "trigger_text": "t", "body_md": "b"}),
            )
            .await),
        "/id",
    );

    let r = ok(&w.delete(&w.alice.token, &format!("/memories/{fact}")).await);
    assert_eq!((r["table"].as_str(), r["action"].as_str()), (Some("facts"), Some("retracted")));
    let r = ok(&w.delete(&w.alice.token, &format!("/memories/{obs}")).await);
    assert_eq!((r["table"].as_str(), r["action"].as_str()), (Some("observations"), Some("archived")));
    let r = ok(&w.delete(&w.alice.token, &format!("/memories/{proc}")).await);
    assert_eq!((r["table"].as_str(), r["action"].as_str()), (Some("procedures"), Some("retired")));

    let status: String =
        sqlx::query_scalar("SELECT status FROM facts WHERE id = $1").bind(fact).fetch_one(&w.pool).await.unwrap();
    assert_eq!(status, "retracted");
    let archived: bool = sqlx::query_scalar("SELECT archived_at IS NOT NULL FROM observations WHERE id = $1")
        .bind(obs)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert!(archived);
    let status: String =
        sqlx::query_scalar("SELECT status FROM procedures WHERE id = $1").bind(proc).fetch_one(&w.pool).await.unwrap();
    assert_eq!(status, "retired");
    // An archived observation no longer shows up in search.
    let found = ok(&w.get(&w.alice.token, "/search?q=router").await);
    assert!(found["rows"].as_array().unwrap().is_empty(), "{found}");

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM memory_audit WHERE action IN ('fact.retract', 'obs.archive', 'proc.retire') ORDER BY action",
    )
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert_eq!(actions, ["fact.retract", "obs.archive", "proc.retire"]);

    // Forgetting again changes nothing and adds no audit row.
    ok(&w.delete(&w.alice.token, &format!("/memories/{fact}")).await);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_audit WHERE action = 'fact.retract'")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn forget_records_the_reason_in_the_audit_log(pool: PgPool) {
    let w = world(pool).await;
    let id = uuid_of(&ok(&w.post(&w.alice.token, "/memories", fact_body("prefs.shell", "fish")).await), "/id");
    ok(&w.delete(&w.alice.token, &format!("/memories/{id}?reason=%20no%20longer%20true%20")).await);
    let reason: Option<String> = sqlx::query_scalar("SELECT reason FROM memory_audit WHERE action = 'fact.retract'")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(reason.as_deref(), Some("no longer true"));

    let other = uuid_of(&ok(&w.post(&w.alice.token, "/memories", fact_body("prefs.os", "linux")).await), "/id");
    let long = "x".repeat(501);
    assert_status(
        &w.delete(&w.alice.token, &format!("/memories/{other}?reason={long}")).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn me_reports_the_token_and_needs_one(pool: PgPool) {
    let w = world(pool).await;
    let me = ok(&w.get(&w.alice.token, "/me").await);
    assert_eq!(me["device_id"], w.alice.device_id);
    assert_eq!(me["token_kind"], "device");
    assert!(!me["scopes"].as_array().unwrap().iter().any(|s| s == "admin"), "{me}");
    let admin = seed_identity_with(&w.pool, TokenKind::Admin, SeedOptions::default()).await;
    let me = ok(&w.get(&admin.token, "/me").await);
    assert!(me["scopes"].as_array().unwrap().iter().any(|s| s == "admin"), "{me}");
    assert_status(
        &w.get("mem_aaaaaaaa_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "/me").await,
        StatusCode::UNAUTHORIZED,
        "invalid_token",
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn forget_respects_visibility_and_roles(pool: PgPool) {
    let w = world(pool).await;
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let carol = seed_identity(&w.pool, TokenKind::Device).await;
    let shared = seed_shared_space(&w.pool, "team", w.alice.user_id, &[(bob.user_id, MembershipRole::Reader)]).await;
    let mut body = fact_body("team.rule", "tests first");
    body["space_id"] = json!(shared);
    let id = uuid_of(&ok(&w.post(&w.alice.token, "/memories", body).await), "/id");

    assert_status(&w.delete(&bob.token, &format!("/memories/{id}")).await, StatusCode::FORBIDDEN, "space_forbidden");
    assert_status(&w.delete(&carol.token, &format!("/memories/{id}")).await, StatusCode::NOT_FOUND, "not_found");
    assert_status(
        &w.delete(&w.alice.token, &format!("/memories/{}", Uuid::now_v7())).await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM facts WHERE id = $1").bind(id).fetch_one(&w.pool).await.unwrap();
    assert_eq!(status, "active");
}

#[sqlx::test(migrations = "../../migrations")]
async fn hard_delete_needs_admin_and_leaves_an_audit_trail_without_content(pool: PgPool) {
    let w = world(pool).await;
    let admin = seed_identity_with(&w.pool, TokenKind::Admin, SeedOptions::default()).await;
    let obs = uuid_of(
        &ok(&w
            .post(
                &admin.token,
                "/memories",
                json!({"kind": "observation", "title": "Rotate the secret token", "narrative": "hunter2"}),
            )
            .await),
        "/id",
    );
    sqlx::query(
        "INSERT INTO embeddings (space_id, owner_table, owner_id, part, model, embedding, scope_kind, scope_key)
         VALUES ($1, 'observations', $2, 'title', 'bge-m3', $3::vector, 'global', '')",
    )
    .bind(admin.space_id)
    .bind(obs)
    .bind(format!("[{}]", vec!["0.1"; 1024].join(",")))
    .execute(&w.pool)
    .await
    .unwrap();

    // A device token cannot hard delete; nothing changes.
    let device = seed_identity(&w.pool, TokenKind::Device).await;
    assert_status(
        &w.delete(&device.token, &format!("/memories/{obs}?hard=true")).await,
        StatusCode::FORBIDDEN,
        "insufficient_scope",
    );
    assert_eq!(w.count("SELECT count(*) FROM observations WHERE id = $1", obs).await, 1);

    let r = ok(&w.delete(&admin.token, &format!("/memories/{obs}?hard=true")).await);
    assert_eq!(r["action"], "deleted");
    assert_eq!(w.count("SELECT count(*) FROM observations WHERE id = $1", obs).await, 0);
    assert_eq!(w.count("SELECT count(*) FROM embeddings WHERE owner_id = $1", obs).await, 0);
    let rows: Vec<(String, Option<Value>, Option<Value>)> =
        sqlx::query_as("SELECT action, before, after FROM memory_audit WHERE target_id = $1 ORDER BY created_at, id")
            .bind(obs)
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["obs.create", "delete.hard"]);
    for (_, before, after) in &rows {
        let text = format!("{before:?}{after:?}");
        assert!(!text.contains("hunter2") && !text.contains("Rotate"), "audit keeps content: {text}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn hard_delete_of_an_observation_clears_references(pool: PgPool) {
    let w = world(pool).await;
    let admin = seed_identity_with(&w.pool, TokenKind::Admin, SeedOptions::default()).await;
    let obs = uuid_of(
        &ok(&w.post(&admin.token, "/memories", json!({"kind": "observation", "title": "evidence"})).await),
        "/id",
    );
    let fact = uuid_of(&ok(&w.post(&admin.token, "/memories", fact_body("k", "v")).await), "/id");
    sqlx::query("UPDATE facts SET evidence_observation_ids = ARRAY[$2]::uuid[] WHERE id = $1")
        .bind(fact)
        .bind(obs)
        .execute(&w.pool)
        .await
        .unwrap();
    ok(&w.delete(&admin.token, &format!("/memories/{obs}?hard=true")).await);
    let evidence: Vec<Uuid> = sqlx::query_scalar("SELECT evidence_observation_ids FROM facts WHERE id = $1")
        .bind(fact)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert!(evidence.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn forgetting_a_fact_drops_the_cached_profile_of_its_scope(pool: PgPool) {
    let w = world(pool).await;
    let admin = seed_identity_with(&w.pool, TokenKind::Admin, SeedOptions::default()).await;
    let fact = uuid_of(&ok(&w.post(&admin.token, "/memories", fact_body("wifi", "hunter2")).await), "/id");
    let put_profile = || async {
        sqlx::query(
            "INSERT INTO profiles (space_id, scope_kind, scope_key, body_md, token_estimate, facts_version)
             VALUES ($1, 'global', '', '- alex: wifi = hunter2', 5, 0)
             ON CONFLICT DO NOTHING",
        )
        .bind(admin.space_id)
        .execute(&w.pool)
        .await
        .unwrap();
    };
    let profiles = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiles WHERE space_id = $1")
            .bind(admin.space_id)
            .fetch_one(&w.pool)
            .await
            .unwrap()
    };

    put_profile().await;
    ok(&w.delete(&admin.token, &format!("/memories/{fact}")).await);
    assert_eq!(profiles().await, 0, "soft delete drops the profile that rendered the value");

    put_profile().await;
    // work packets that carry the fact keep its content too
    sqlx::query(
        "INSERT INTO work_packets (id, space_id, kind, prompt_ref, model_hint, inputs, output_schema, status,
                                   idempotency_key, result)
         VALUES ($1, $2, 'consolidate_facts', 'consolidate.facts@v1', 'sonnet', $3, 'consolidate.facts.v1', 'done',
                 'k1', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(admin.space_id)
    .bind(json!({"facts": [{"id": fact, "value_text": "hunter2"}]}))
    .execute(&w.pool)
    .await
    .unwrap();
    ok(&w.delete(&admin.token, &format!("/memories/{fact}?hard=true")).await);
    assert_eq!(profiles().await, 0);
    let packet: (Value, Option<Value>) =
        sqlx::query_as("SELECT inputs, result FROM work_packets WHERE idempotency_key = 'k1'")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!(packet, (json!({}), None));

    // forgetting an unrelated kind of memory leaves profiles alone
    put_profile().await;
    let obs =
        uuid_of(&ok(&w.post(&admin.token, "/memories", json!({"kind": "observation", "title": "t"})).await), "/id");
    ok(&w.delete(&admin.token, &format!("/memories/{obs}")).await);
    assert_eq!(profiles().await, 1);
}
