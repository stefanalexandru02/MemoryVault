//! Work recalled: which memories count as recalled (full bodies that reached the model, once per session) and the
//! `work` block of `GET /v1/stats/recall`.

use axum::Router;
use axum::http::{Method, StatusCode};
use memvault_core::auth::scopes::TokenKind;
use memvault_core::schemas::common::MembershipRole;
use memvault_core::test_support::{app, seed_identity, seed_shared_space, send};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::*;

async fn work(app: &Router, token: &str, query: &str) -> Value {
    let resp = send(app, Method::GET, &format!("/v1/stats/recall{query}"), Some(token), None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    resp.json()["work"].clone()
}

async fn get(app: &Router, token: &str, body: Value) {
    let resp = send(app, Method::POST, "/v1/memories/get", Some(token), Some(body)).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
}

async fn set_work(pool: &PgPool, table: &str, id: Uuid, tokens: i32) {
    sqlx::query(&format!("UPDATE {table} SET work_tokens = $2 WHERE id = $1"))
        .bind(id)
        .bind(tokens)
        .execute(pool)
        .await
        .unwrap();
}

/// (memories_recalled, work_tokens_recalled, memories_without_work_estimate)
fn counts(work: &Value) -> (i64, i64, i64) {
    let n = |k: &str| work[k].as_i64().unwrap();
    (n("memories_recalled"), n("work_tokens_recalled"), n("memories_without_work_estimate"))
}

#[sqlx::test(migrations = "../../migrations")]
async fn memory_get_counts_once_per_session_and_index_rows_do_not(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let fixed = insert_obs(&w, Obs::new(space, "Fixed the parser token loss", "parser dropped the last token")).await;
    let other = insert_obs(&w, Obs::new(space, "Parser config lives in toml", "parser reads parser.toml")).await;
    set_work(&w.pool, "observations", fixed, 1200).await;
    let app = app(w.state.clone(), memvault_recall::router());
    let token = w.alice.token.as_str();
    let session = insert_session(&w.pool, space, w.alice.client_id, PROJECT, 5.0).await;

    let empty = work(&app, token, "").await;
    assert_eq!(
        empty,
        json!({"tokens_served": 0, "memories_recalled": 0, "work_tokens_recalled": 0,
               "memories_without_work_estimate": 0, "ratio": null})
    );

    // Index rows are titles: search, the timeline and the session-start index recall nothing.
    for uri in ["/v1/search?q=parser".to_string(), format!("/v1/timeline?anchor_id={fixed}")] {
        assert_eq!(send(&app, Method::GET, &uri, Some(token), None).await.status, StatusCode::OK, "{uri}");
    }
    let ctx = send(&app, Method::GET, &format!("/v1/context?project_key={PROJECT}"), Some(token), None).await;
    assert!(ctx.text().contains("Fixed the parser"), "{}", ctx.text());
    let after_index = work(&app, token, "").await;
    assert_eq!(counts(&after_index), (0, 0, 0), "{after_index}");
    let served = after_index["tokens_served"].as_i64().unwrap();
    assert!(served > 0, "the session start was served: {after_index}");

    // memory_get bodies count, once per session however often they are fetched.
    get(&app, token, json!({"ids": [fixed, other]})).await;
    let first = work(&app, token, "").await;
    assert_eq!(counts(&first), (2, 1200, 1), "{first}");
    get(&app, token, json!({"ids": [fixed]})).await;
    let again = work(&app, token, "").await;
    assert_eq!(counts(&again), (2, 1200, 1), "a second fetch in the session does not count: {again}");
    let served_now = again["tokens_served"].as_i64().unwrap();
    assert!(served_now > first["tokens_served"].as_i64().unwrap(), "but its tokens were served again");
    let ratio = again["ratio"].as_f64().unwrap();
    assert!((ratio - 1200.0 / served_now as f64).abs() < 1e-9, "{again}");
    let sessions: Vec<Option<Uuid>> =
        sqlx::query_scalar("SELECT DISTINCT session_id FROM access_log WHERE action = 'get'")
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert_eq!(sessions, [Some(session)]);

    // A person reading the body in the viewer is neither served to nor recalled by a model.
    get(&app, token, json!({"ids": [other], "display": true})).await;
    assert_eq!(work(&app, token, "").await, again);
    let gets: i64 =
        sqlx::query_scalar("SELECT count(*) FROM retrieval_log WHERE mode = 'get'").fetch_one(&w.pool).await.unwrap();
    assert_eq!(gets, 2);

    // Outside a session the memory counts once per user and day.
    sqlx::query("UPDATE sessions SET ended_at = now() WHERE id = $1").bind(session).execute(&w.pool).await.unwrap();
    for _ in 0..2 {
        get(&app, token, json!({"ids": [fixed]})).await;
    }
    assert_eq!(counts(&work(&app, token, "").await), (3, 2400, 1));
}

#[sqlx::test(migrations = "../../migrations")]
async fn session_start_and_recall_count_full_bodies_only(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let evidence = insert_obs(&w, Obs::new(space, "Chose pgvector over Chroma", "one store, RLS covers vectors")).await;
    set_work(&w.pool, "observations", evidence, 300).await;
    let mut pinned = Fact::new(space, "prefers.vector_store", "pgvector for personal projects");
    pinned.pinned = true;
    pinned.evidence = vec![evidence];
    let fact = insert_fact(&w, pinned).await;
    let procedure =
        insert_procedure(&w, space, "Deploy memory-api", "deploy", "run migrations Job first, then roll Deployment")
            .await;
    sqlx::query("UPDATE procedures SET source_observation_ids = $2 WHERE id = $1")
        .bind(procedure)
        .bind(vec![evidence, evidence])
        .execute(&w.pool)
        .await
        .unwrap();
    let derived: Vec<Option<i32>> =
        sqlx::query_scalar("SELECT work_tokens FROM facts UNION ALL SELECT work_tokens FROM procedures")
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert_eq!(derived, [Some(300), Some(300)], "the distinct evidence observations, summed");
    let app = app(w.state.clone(), memvault_recall::router());
    let token = w.alice.token.as_str();
    insert_session(&w.pool, space, w.alice.client_id, PROJECT, 1.0).await;

    // The block renders the pinned fact and the procedure in full, the observation as an index row.
    let ctx = format!("/v1/context?project_key={PROJECT}");
    let block = send(&app, Method::GET, &ctx, Some(token), None).await.text();
    assert!(block.contains("Deploy memory-api") && block.contains("Chose pgvector"), "{block}");
    let recalled = |rows: Vec<Vec<Uuid>>| -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = rows.concat();
        ids.sort();
        ids
    };
    let mut want = vec![fact, procedure];
    want.sort();
    let rows: Vec<Vec<Uuid>> = sqlx::query_scalar("SELECT recalled_ids FROM access_log WHERE action = 'context'")
        .fetch_all(&w.pool)
        .await
        .unwrap();
    assert_eq!(recalled(rows), want);
    assert_eq!(counts(&work(&app, token, "").await), (2, 600, 0));

    // A cache hit renders the same block: logged, but the session already counted them.
    send(&app, Method::GET, &ctx, Some(token), None).await;
    let rows: Vec<Vec<Uuid>> = sqlx::query_scalar(
        "SELECT recalled_ids FROM access_log WHERE action = 'context' ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert_eq!(recalled(rows), want, "the hit logs what the cached block renders");
    assert_eq!(counts(&work(&app, token, "").await), (2, 600, 0));

    // /v1/recall: the expanded observation counts; without expansion it is an index row.
    let body = |expand: u32| {
        json!({"query": "pgvector chroma", "expand_top_observations": expand,
               "scopes": [{"kind": "global"}, {"kind": "project", "key": PROJECT}]})
    };
    let resp = send(&app, Method::POST, "/v1/recall", Some(token), Some(body(0))).await;
    assert_eq!(resp.json()["slots"]["observations"][0]["expanded"], false);
    assert_eq!(counts(&work(&app, token, "").await), (2, 600, 0));
    let resp = send(&app, Method::POST, "/v1/recall", Some(token), Some(body(1))).await;
    assert_eq!(resp.json()["slots"]["observations"][0]["expanded"], true);
    assert_eq!(counts(&work(&app, token, "").await), (3, 900, 0));
}

#[sqlx::test(migrations = "../../migrations")]
async fn work_stats_are_per_user_and_all_users_for_admins(pool: PgPool) {
    let w = world(pool).await;
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let admin = seed_identity(&w.pool, TokenKind::Admin).await;
    let team = seed_shared_space(&w.pool, "team", w.alice.user_id, &[(bob.user_id, MembershipRole::Writer)]).await;
    let shared = insert_obs(&w, Obs::new(team, "Team runbook moved", "the runbook is in the wiki")).await;
    set_work(&w.pool, "observations", shared, 500).await;
    let app = app(w.state.clone(), memvault_recall::router());

    // Alice reads a memory of the shared space; Bob, a member of it, does not see her numbers.
    get(&app, &w.alice.token, json!({"ids": [shared]})).await;
    assert_eq!(counts(&work(&app, &w.alice.token, "").await), (1, 500, 0));
    let bobs = work(&app, &bob.token, "").await;
    assert_eq!(counts(&bobs), (0, 0, 0), "{bobs}");
    assert_eq!(bobs["tokens_served"], 0);

    // Bob reads it too: each user counts their own.
    get(&app, &bob.token, json!({"ids": [shared]})).await;
    assert_eq!(counts(&work(&app, &bob.token, "").await), (1, 500, 0));
    let all = work(&app, &admin.token, "?all=true").await;
    assert_eq!(counts(&all), (2, 1000, 0), "{all}");
    assert_eq!(counts(&work(&app, &admin.token, "").await), (0, 0, 0), "the admin's own calls by default");
    let resp = send(&app, Method::GET, "/v1/stats/recall?all=true", Some(&bob.token), None).await;
    assert_eq!((resp.status, resp.error_code().as_deref()), (StatusCode::FORBIDDEN, Some("insufficient_scope")));

    // The window bounds it like the rest of the stats.
    sqlx::query("UPDATE access_log SET created_at = now() - interval '3 days'").execute(&w.pool).await.unwrap();
    sqlx::query("UPDATE retrieval_log SET created_at = now() - interval '3 days'").execute(&w.pool).await.unwrap();
    let old = work(&app, &w.alice.token, "?days=2").await;
    assert_eq!((counts(&old), old["ratio"].clone()), ((0, 0, 0), Value::Null));
}
