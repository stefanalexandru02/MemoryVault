//! `POST /v1/memories/{id}/feedback`: per-memory verdicts, their effect and their bounds.

use axum::http::StatusCode;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::schemas::common::MembershipRole;
use memvault_core::test_support::{seed_identity, seed_shared_space};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use uuid::Uuid;

use crate::common::{World, assert_status, fact_body, ok, uuid_of, world};

impl World {
    async fn feedback(&self, token: &str, id: Uuid, body: Value) -> Value {
        ok(&self.post(token, &format!("/memories/{id}/feedback"), body).await)
    }

    async fn fact(&self, predicate: &str, value: &str) -> Uuid {
        uuid_of(&ok(&self.post(&self.alice.token, "/memories", fact_body(predicate, value)).await), "/id")
    }

    async fn fact_state(&self, id: Uuid) -> (f32, bool, bool) {
        sqlx::query_as(
            "SELECT confidence, disputed_at IS NOT NULL, last_confirmed_at > now() - interval '1 minute'
               FROM facts WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn verdicts_steer_confidence_once_each_and_are_audited(pool: PgPool) {
    let w = world(pool).await;
    let id = w.fact("prefers.editor", "neovim").await;
    sqlx::query("UPDATE facts SET confidence = 0.6, last_confirmed_at = now() - interval '200 days' WHERE id = $1")
        .bind(id)
        .execute(&w.pool)
        .await
        .unwrap();

    let helpful = w.feedback(&w.alice.token, id, json!({"verdict": "helpful"})).await;
    assert_eq!(helpful["applied"], true);
    assert_eq!(helpful["counts"], json!({"helpful": 1, "wrong": 0, "outdated": 0}));
    assert!((helpful["confidence"].as_f64().unwrap() - 0.65).abs() < 1e-6, "{helpful}");
    assert_eq!(w.fact_state(id).await, (0.65, false, true), "helpful confirms the fact");

    // The same verdict again changes nothing, however often it is sent.
    for _ in 0..3 {
        let again = w.feedback(&w.alice.token, id, json!({"verdict": "helpful", "note": "again"})).await;
        assert_eq!(again["applied"], false);
        assert_eq!(again["counts"]["helpful"], 1);
    }
    assert_eq!(w.fact_state(id).await.0, 0.65);

    let outdated = w.feedback(&w.alice.token, id, json!({"verdict": "outdated", "note": "  moved to helix  "})).await;
    assert_eq!((outdated["applied"].clone(), outdated["queued"].clone()), (json!(true), json!(false)));
    assert_eq!(w.fact_state(id).await, (0.55, false, true));

    let rows: Vec<(String, Option<String>, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT verdict, note, user_id, client_id FROM memory_feedback WHERE target_id = $1
          ORDER BY created_at",
    )
    .bind(id)
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "a repeat adds no row");
    assert_eq!(rows[1], ("outdated".into(), Some("moved to helix".into()), w.alice.user_id, Some(w.alice.client_id)));
    assert_eq!(rows[0].1, None);

    let audit: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT action, reason, actor FROM memory_audit WHERE target_id = $1 AND action = 'fact.feedback'
          ORDER BY seq",
    )
    .bind(id)
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert_eq!(
        audit,
        [
            ("fact.feedback".to_string(), Some("helpful".to_string()), format!("client:{}", w.alice.client_id)),
            (
                "fact.feedback".to_string(),
                Some("outdated: moved to helix".to_string()),
                format!("client:{}", w.alice.client_id)
            ),
        ]
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn confidence_stays_within_bounds(pool: PgPool) {
    let w = world(pool).await;
    let id = w.fact("prefers.editor", "neovim").await;
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let carol = seed_identity(&w.pool, TokenKind::Device).await;
    let members = [(bob.user_id, MembershipRole::Writer), (carol.user_id, MembershipRole::Writer)];
    let shared = seed_shared_space(&w.pool, "team", w.alice.user_id, &members).await;
    let mut body = fact_body("team.rule", "tests first");
    body["space_id"] = json!(shared);
    let team = uuid_of(&ok(&w.post(&w.alice.token, "/memories", body).await), "/id");

    let up = w.feedback(&w.alice.token, id, json!({"verdict": "helpful"})).await;
    assert_eq!(up["confidence"], 0.98, "capped");
    // Two users give both negative verdicts: each counts once, and the floor holds.
    for token in [&w.alice.token, &bob.token] {
        for verdict in ["wrong", "outdated"] {
            w.feedback(token, team, json!({"verdict": verdict})).await;
        }
    }
    sqlx::query("UPDATE facts SET confidence = 0.1 WHERE id = $1").bind(team).execute(&w.pool).await.unwrap();
    let low = w.feedback(&carol.token, team, json!({"verdict": "wrong"})).await;
    assert_eq!(low["confidence"], 0.05, "floored");
    assert_eq!(low["counts"], json!({"helpful": 0, "wrong": 3, "outdated": 2}), "each user counts once per verdict");
}

#[sqlx::test(migrations = "../../migrations")]
async fn net_negative_feedback_queues_a_fact_and_helpful_can_release_it(pool: PgPool) {
    let w = world(pool).await;
    let id = w.fact("prefers.editor", "neovim").await;

    let outdated = w.feedback(&w.alice.token, id, json!({"verdict": "outdated"})).await;
    assert_eq!(outdated["queued"], false, "one outdated is not enough");
    let again = w.feedback(&w.alice.token, id, json!({"verdict": "outdated"})).await;
    assert_eq!(again["queued"], false, "and a repeat does not add up");
    let wrong = w.feedback(&w.alice.token, id, json!({"verdict": "wrong"})).await;
    assert_eq!(wrong["queued"], true);
    assert!((wrong["confidence"].as_f64().unwrap() - 0.65).abs() < 1e-6, "{wrong}");
    assert!(w.fact_state(id).await.1);
    let status: String =
        sqlx::query_scalar("SELECT status FROM facts WHERE id = $1").bind(id).fetch_one(&w.pool).await.unwrap();
    assert_eq!(status, "active", "feedback itself retracts nothing");

    // A lone `wrong` queues the fact too, and one `helpful` takes it back out.
    let other = w.fact("prefers.shell", "fish").await;
    assert_eq!(w.feedback(&w.alice.token, other, json!({"verdict": "wrong"})).await["queued"], true);
    assert_eq!(w.feedback(&w.alice.token, other, json!({"verdict": "helpful"})).await["queued"], false);
    assert!(!w.fact_state(other).await.1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn pinned_facts_keep_their_confidence_and_are_never_queued(pool: PgPool) {
    let w = world(pool).await;
    let mut body = fact_body("prefers.shell", "fish");
    body["pinned"] = json!(true);
    let id = uuid_of(&ok(&w.post(&w.alice.token, "/memories", body).await), "/id");
    let wrong = w.feedback(&w.alice.token, id, json!({"verdict": "wrong"})).await;
    assert_eq!(wrong["applied"], true);
    assert_eq!((wrong["confidence"].clone(), wrong["queued"].clone()), (json!(0.95), json!(false)));
    assert_eq!(wrong["counts"]["wrong"], 1, "the verdict is still recorded");
}

#[sqlx::test(migrations = "../../migrations")]
async fn observations_and_procedures_take_the_verdict_in_their_own_terms(pool: PgPool) {
    let w = world(pool).await;
    let obs = uuid_of(
        &ok(&w
            .post(
                &w.alice.token,
                "/memories",
                json!({"kind": "observation", "title": "Router reboots", "importance": 0.7}),
            )
            .await),
        "/id",
    );
    let proc = uuid_of(
        &ok(&w
            .post(
                &w.alice.token,
                "/memories",
                json!({"kind": "procedure", "name": "deploy", "task_class": "ops", "trigger_text": "deploy", "body_md": "run it"}),
            )
            .await),
        "/id",
    );

    let wrong = w.feedback(&w.alice.token, obs, json!({"verdict": "wrong"})).await;
    assert_eq!(wrong["table"], "observations");
    assert_eq!((wrong["confidence"].clone(), wrong["queued"].clone()), (Value::Null, json!(false)));
    let importance: f32 = sqlx::query_scalar("SELECT importance FROM observations WHERE id = $1")
        .bind(obs)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert!((importance - 0.5).abs() < 1e-6);

    w.feedback(&w.alice.token, proc, json!({"verdict": "helpful"})).await;
    w.feedback(&w.alice.token, proc, json!({"verdict": "helpful"})).await;
    w.feedback(&w.alice.token, proc, json!({"verdict": "outdated"})).await;
    let (ok_count, failed): (i32, i32) =
        sqlx::query_as("SELECT success_count, failure_count FROM procedures WHERE id = $1")
            .bind(proc)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!((ok_count, failed), (1, 1), "a repeated verdict does not count twice");
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM memory_audit WHERE action LIKE '%.feedback' AND target_id = ANY($1) ORDER BY seq",
    )
    .bind(vec![obs, proc])
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert_eq!(actions, ["obs.feedback", "proc.feedback", "proc.feedback"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn feedback_respects_visibility_roles_and_input_rules(pool: PgPool) {
    let w = world(pool).await;
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let carol = seed_identity(&w.pool, TokenKind::Device).await;
    let shared = seed_shared_space(&w.pool, "team", w.alice.user_id, &[(bob.user_id, MembershipRole::Reader)]).await;
    let mut body = fact_body("team.rule", "tests first");
    body["space_id"] = json!(shared);
    let id = uuid_of(&ok(&w.post(&w.alice.token, "/memories", body).await), "/id");
    let path = format!("/memories/{id}/feedback");

    assert_status(
        &w.post(&bob.token, &path, json!({"verdict": "wrong"})).await,
        StatusCode::FORBIDDEN,
        "space_forbidden",
    );
    assert_status(&w.post(&carol.token, &path, json!({"verdict": "wrong"})).await, StatusCode::NOT_FOUND, "not_found");
    let missing = format!("/memories/{}/feedback", Uuid::now_v7());
    assert_status(
        &w.post(&w.alice.token, &missing, json!({"verdict": "wrong"})).await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    let runner = seed_identity(&w.pool, TokenKind::Runner).await;
    assert_status(
        &w.post(&runner.token, &path, json!({"verdict": "wrong"})).await,
        StatusCode::FORBIDDEN,
        "insufficient_scope",
    );
    let long = json!({"verdict": "wrong", "note": "x".repeat(501)});
    assert_status(&w.post(&w.alice.token, &path, long).await, StatusCode::UNPROCESSABLE_ENTITY, "invalid_request");
    assert_status(
        &w.post(&w.alice.token, &path, json!({"verdict": "meh"})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
    );

    let summary = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, request, extracted_by)
         VALUES ($1, $2, 'global', '', 'day', 'x', '{}'::jsonb)",
    )
    .bind(summary)
    .bind(w.alice.space_id)
    .execute(&w.pool)
    .await
    .unwrap();
    let on_summary =
        w.post(&w.alice.token, &format!("/memories/{summary}/feedback"), json!({"verdict": "wrong"})).await;
    assert_status(&on_summary, StatusCode::UNPROCESSABLE_ENTITY, "invalid_request");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_feedback").fetch_one(&w.pool).await.unwrap();
    assert_eq!(rows, 0, "nothing was stored by the rejected calls");
}

#[sqlx::test(migrations = "../../migrations")]
async fn feedback_drops_the_cached_profile_and_signals_the_scope(pool: PgPool) {
    let w = world(pool).await;
    let id = w.fact("timezone", "Europe/Bucharest").await;
    sqlx::query(
        "INSERT INTO profiles (space_id, scope_kind, scope_key, body_md, token_estimate, facts_version)
         VALUES ($1, 'global', '', '- alex: timezone = Europe/Bucharest', 5, 0)",
    )
    .bind(w.alice.space_id)
    .execute(&w.pool)
    .await
    .unwrap();
    let mut listener = PgListener::connect_with(&w.pool).await.unwrap();
    listener.listen("memory_invalidate").await.unwrap();

    w.feedback(&w.alice.token, id, json!({"verdict": "outdated"})).await;
    let note =
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv()).await.expect("notified").unwrap();
    assert_eq!(note.payload(), format!("{}:global:", w.alice.space_id));
    let profiles: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles").fetch_one(&w.pool).await.unwrap();
    assert_eq!(profiles, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn get_reports_counts_and_cautions_and_hard_delete_removes_the_verdicts(pool: PgPool) {
    let w = world(pool).await;
    let admin = seed_identity(&w.pool, TokenKind::Admin).await;
    let id = uuid_of(&ok(&w.post(&admin.token, "/memories", fact_body("prefers.editor", "neovim")).await), "/id");
    w.feedback(&admin.token, id, json!({"verdict": "wrong", "note": "private detail"})).await;
    let got = ok(&w.post(&admin.token, "/memories/get", json!({"ids": [id]})).await);
    assert_eq!(got["feedback"][id.to_string()], json!({"helpful": 0, "wrong": 1, "outdated": 0}));
    assert_eq!(got["items"][0]["caution"], "disputed", "queued by the wrong verdict");

    ok(&w.delete(&admin.token, &format!("/memories/{id}?hard=true")).await);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_feedback").fetch_one(&w.pool).await.unwrap();
    assert_eq!(rows, 0, "notes go with the memory");
}
