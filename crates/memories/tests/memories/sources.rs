//! `POST`, `GET` and `DELETE /v1/sources`: chunking, idempotency, replacement, removal, limits, row security.

use axum::http::StatusCode;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::schemas::common::MembershipRole;
use memvault_core::test_support::{seed_identity, seed_shared_space};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{PROJECT, World, assert_status, ok, world};

const URI: &str = "docs/adr/0003-postgres.md";

fn words(n: usize) -> String {
    vec!["storage"; n].join(" ")
}

/// A markdown document that makes several chunks: an intro and two long sections.
fn adr(decision: &str) -> String {
    format!(
        "# ADR 3: use Postgres\n\nWe need one database.\n\n## Decision\n\n{decision}\n\n{}\n\n## Consequences\n\n{}\n",
        words(400),
        words(400)
    )
}

fn body(uri: &str, content: &str, hash: &str) -> Value {
    json!({
        "scope": {"kind": "project", "key": PROJECT}, "uri": uri, "content_type": "markdown",
        "content": content, "content_hash": hash,
    })
}

async fn live_chunks(w: &World, uri: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM documents WHERE source_uri = $1 AND retired_at IS NULL")
        .bind(uri)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn embed_jobs(w: &World) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM jobs WHERE type = 'embed' AND payload->>'owner_table' = 'documents'")
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_source_is_chunked_with_provenance_jobs_and_audit(pool: PgPool) {
    let w = world(pool).await;
    let resp = w.post(&w.alice.token, "/sources", body(URI, &adr("Use Postgres."), "h1")).await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let out = resp.json();
    let chunks = out["chunks"].as_i64().unwrap();
    assert!(chunks >= 2, "{out}");
    assert_eq!(
        (out["status"].as_str(), out["retired"].as_i64(), out["uri"].as_str()),
        (Some("created"), Some(0), Some(URI))
    );

    type Row = (i32, Vec<String>, String, String, String, i32, Option<Uuid>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT chunk_index, heading_path, title, content_hash, content_type, token_estimate, ingested_by
           FROM documents WHERE source_uri = $1 ORDER BY chunk_index",
    )
    .bind(URI)
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert_eq!(rows.len() as i64, chunks);
    assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), (0..chunks as i32).collect::<Vec<_>>());
    assert_eq!(rows[0].1, vec!["ADR 3: use Postgres"]);
    assert!(rows.iter().any(|r| r.1 == ["ADR 3: use Postgres", "Consequences"]), "{rows:?}");
    for r in &rows {
        assert_eq!((r.2.as_str(), r.3.as_str(), r.4.as_str()), ("ADR 3: use Postgres", "h1", "markdown"));
        assert!((1..=820).contains(&r.5), "token estimate {}", r.5);
        assert_eq!(r.6, Some(w.alice.client_id));
    }
    assert_eq!(embed_jobs(&w).await, chunks);
    let (action, actor): (String, String) =
        sqlx::query_as("SELECT action, actor FROM memory_audit WHERE target_table = 'documents'")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!((action.as_str(), actor.as_str()), ("doc.ingest", format!("client:{}", w.alice.client_id).as_str()));
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_same_hash_is_a_no_op_and_a_new_hash_replaces_the_chunks(pool: PgPool) {
    let w = world(pool).await;
    let first = ok(&w.post(&w.alice.token, "/sources", body(URI, &adr("Use Postgres."), "h1")).await);
    let jobs = embed_jobs(&w).await;

    // The same hash changes nothing, whatever the content says.
    let again = w.post(&w.alice.token, "/sources", body(URI, &adr("Use MySQL."), "h1")).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.text());
    assert_eq!(again.json()["status"], "unchanged");
    assert_eq!(again.json()["chunks"], first["chunks"]);
    assert_eq!(embed_jobs(&w).await, jobs);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM documents").fetch_one(&w.pool).await.unwrap();
    assert_eq!(total, first["chunks"].as_i64().unwrap());

    // Embeddings of the old chunks go away with them.
    sqlx::query(
        "INSERT INTO embeddings (space_id, owner_table, owner_id, part, model, embedding, scope_kind, scope_key)
         SELECT space_id, 'documents', id, 'body:0', 'bge-m3', array_fill(0.1::real, ARRAY[1024])::vector, scope_kind, scope_key
           FROM documents",
    )
    .execute(&w.pool)
    .await
    .unwrap();

    let replaced = w.post(&w.alice.token, "/sources", body(URI, &adr("Use MySQL."), "h2")).await;
    assert_eq!(replaced.status, StatusCode::CREATED, "{}", replaced.text());
    let out = replaced.json();
    assert_eq!(out["status"], "replaced");
    assert_eq!(out["retired"], first["chunks"]);
    assert_eq!(live_chunks(&w, URI).await, out["chunks"].as_i64().unwrap());
    let retired: i64 = sqlx::query_scalar("SELECT count(*) FROM documents WHERE retired_at IS NOT NULL")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(retired, first["chunks"].as_i64().unwrap(), "old rows are retired, not left live");
    let hashes: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT content_hash FROM documents WHERE retired_at IS NULL")
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert_eq!(hashes, ["h2"]);
    let stale: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM embeddings e JOIN documents d ON d.id = e.owner_id WHERE d.retired_at IS NOT NULL",
    )
    .fetch_one(&w.pool)
    .await
    .unwrap();
    assert_eq!(stale, 0, "retired chunks keep no embeddings");
    assert_eq!(embed_jobs(&w).await, jobs + out["chunks"].as_i64().unwrap());
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM memory_audit ORDER BY seq").fetch_all(&w.pool).await.unwrap();
    assert_eq!(actions, ["doc.ingest", "doc.replace"]);

    // Back to the first version: replaced again, and a smaller document leaves fewer chunks.
    let small = w.post(&w.alice.token, "/sources", body(URI, "# Tiny\n\nshort", "h3")).await;
    assert_eq!(small.json()["chunks"], 1);
    assert_eq!(live_chunks(&w, URI).await, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_identical_posts_leave_one_copy(pool: PgPool) {
    let w = world(pool).await;
    let post = || w.post(&w.alice.token, "/sources", body(URI, &adr("Use Postgres."), "h1"));
    let (a, b) = futures::join!(post(), post());
    let mut statuses =
        [a.json()["status"].as_str().unwrap().to_string(), b.json()["status"].as_str().unwrap().to_string()];
    statuses.sort();
    assert_eq!(statuses, ["created", "unchanged"], "{} {}", a.text(), b.text());
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM documents").fetch_one(&w.pool).await.unwrap();
    assert_eq!(total, live_chunks(&w, URI).await);
}

#[sqlx::test(migrations = "../../migrations")]
async fn sources_are_listed_by_scope_and_removed(pool: PgPool) {
    let w = world(pool).await;
    ok(&w.post(&w.alice.token, "/sources", body(URI, &adr("Use Postgres."), "h1")).await);
    ok(&w.post(&w.alice.token, "/sources", body("README.md", "# Homelab\n\nNotes.", "r1")).await);
    let global = json!({"scope": {"kind": "global"}, "uri": "~/notes/vim.txt", "content_type": "text", "content": "use :wq", "content_hash": "g1"});
    ok(&w.post(&w.alice.token, "/sources", global).await);
    ok(&w.post(&w.alice.token, "/sources", body(URI, &adr("Use MySQL."), "h2")).await);

    let listed = ok(&w.get(&w.alice.token, "/sources").await);
    let rows = listed["sources"].as_array().unwrap();
    let mut uris: Vec<&str> = rows.iter().map(|s| s["uri"].as_str().unwrap()).collect();
    uris.sort();
    assert_eq!(uris, ["README.md", "docs/adr/0003-postgres.md", "~/notes/vim.txt"], "{listed}");
    let adr_row = rows.iter().find(|s| s["uri"] == URI).unwrap();
    assert_eq!(adr_row["title"], "ADR 3: use Postgres");
    assert_eq!(adr_row["content_hash"], "h2");
    assert_eq!(adr_row["content_type"], "markdown");
    assert_eq!(adr_row["scope"], json!({"kind": "project", "key": PROJECT}));
    assert_eq!(adr_row["chunks"].as_i64().unwrap(), live_chunks(&w, URI).await);
    assert!(adr_row["ingested_at"].is_string());

    let project = ok(&w.get(&w.alice.token, &format!("/sources?scope=project:{PROJECT}")).await);
    assert_eq!(project["sources"].as_array().unwrap().len(), 2);
    let global = ok(&w.get(&w.alice.token, "/sources?scope=global").await);
    assert_eq!(global["sources"].as_array().unwrap().len(), 1);
    assert_status(
        &w.get(&w.alice.token, "/sources?scope=device:x").await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
    );

    let removed = ok(&w.delete(&w.alice.token, &format!("/sources?uri={URI}&scope=project:{PROJECT}")).await);
    assert!(removed["deleted"].as_i64().unwrap() > 1, "{removed}");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM documents WHERE source_uri = $1")
        .bind(URI)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "retired chunks go too");
    let uris = ok(&w.get(&w.alice.token, "/sources").await);
    assert_eq!(uris["sources"].as_array().unwrap().len(), 2);
    let again = w.delete(&w.alice.token, &format!("/sources?uri={URI}&scope=project:{PROJECT}")).await;
    assert_status(&again, StatusCode::NOT_FOUND, "not_found");
    // The same uri in another scope is another source.
    let other = w.delete(&w.alice.token, "/sources?uri=README.md&scope=global").await;
    assert_status(&other, StatusCode::NOT_FOUND, "not_found");
    let last_audit: String = sqlx::query_scalar("SELECT action FROM memory_audit ORDER BY seq DESC LIMIT 1")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(last_audit, "doc.delete");
}

#[sqlx::test(migrations = "../../migrations")]
async fn requests_are_validated_and_size_limited(pool: PgPool) {
    let w = world(pool).await;
    let invalid = |resp, what: &str| {
        assert_status(&resp, StatusCode::UNPROCESSABLE_ENTITY, "invalid_request");
        assert!(resp.text().contains(what), "{what}: {}", resp.text());
    };
    let mut b = body(URI, "text", "h1");
    b["scope"] = json!({"kind": "device", "key": "laptop"});
    invalid(w.post(&w.alice.token, "/sources", b).await, "project or to the global scope");
    let mut b = body(URI, "text", "h1");
    b["scope"] = json!({"kind": "project", "key": ""});
    invalid(w.post(&w.alice.token, "/sources", b).await, "needs a key");
    invalid(w.post(&w.alice.token, "/sources", body(URI, "  \n ", "h1")).await, "content");
    invalid(w.post(&w.alice.token, "/sources", body("", "text", "h1")).await, "uri");
    invalid(w.post(&w.alice.token, "/sources", body(URI, "text", "two words")).await, "content_hash");
    let mut b = body(URI, "text", "h1");
    b["content_type"] = json!("html");
    assert_eq!(w.post(&w.alice.token, "/sources", b).await.status, StatusCode::UNPROCESSABLE_ENTITY);

    let big = "x".repeat(w.state.settings.source_max_bytes + 1);
    let resp = w.post(&w.alice.token, "/sources", body(URI, &big, "h1")).await;
    assert_status(&resp, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    assert_eq!(resp.json()["error"]["retryable"], false);
    let fits = "x ".repeat(w.state.settings.source_max_bytes / 2);
    ok(&w.post(&w.alice.token, "/sources", body("big.txt", &fits, "h1")).await);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM documents WHERE source_uri = $1")
        .bind(URI)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(total, 0, "a rejected request writes nothing");
}

#[sqlx::test(migrations = "../../migrations")]
async fn sources_respect_spaces_and_roles(pool: PgPool) {
    let w = world(pool.clone()).await;
    let bob = seed_identity(&pool, TokenKind::Device).await;
    ok(&w.post(&w.alice.token, "/sources", body(URI, &adr("Use Postgres."), "h1")).await);

    // Bob sees nothing of Alice's, cannot remove it, and his own copy of the uri is a separate source.
    assert_eq!(ok(&w.get(&bob.token, "/sources").await)["sources"], json!([]));
    let del = w.delete(&bob.token, &format!("/sources?uri={URI}&scope=project:{PROJECT}")).await;
    assert_status(&del, StatusCode::NOT_FOUND, "not_found");
    let his = w.post(&bob.token, "/sources", body(URI, &adr("Use Postgres."), "h1")).await;
    assert_eq!(his.status, StatusCode::CREATED, "{}", his.text());
    assert_eq!(his.json()["status"], "created");
    assert_eq!(ok(&w.get(&bob.token, "/sources").await)["sources"].as_array().unwrap().len(), 1);

    // A shared space: the reader lists it but cannot write.
    let shared = seed_shared_space(&pool, "team", w.alice.user_id, &[(bob.user_id, MembershipRole::Reader)]).await;
    let mut b = body("docs/team.md", "# Team\n\nrunbook", "t1");
    b["space_id"] = json!(shared);
    ok(&w.post(&w.alice.token, "/sources", b.clone()).await);
    assert_status(&w.post(&bob.token, "/sources", b).await, StatusCode::FORBIDDEN, "space_forbidden");
    // Without a space the list is that of the default space; the shared one is asked for by id.
    let own = ok(&w.get(&bob.token, "/sources").await);
    assert!(own["sources"].as_array().unwrap().iter().all(|s| s["space_id"] != json!(shared)));
    let only = ok(&w.get(&bob.token, &format!("/sources?space_id={shared}")).await);
    assert_eq!(only["sources"].as_array().unwrap().len(), 1);
    let del =
        w.delete(&bob.token, &format!("/sources?uri=docs/team.md&scope=project:{PROJECT}&space_id={shared}")).await;
    assert_status(&del, StatusCode::FORBIDDEN, "space_forbidden");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_chunk_can_be_forgotten_but_takes_no_feedback(pool: PgPool) {
    let w = world(pool).await;
    ok(&w.post(&w.alice.token, "/sources", body(URI, &adr("Use Postgres."), "h1")).await);
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM documents WHERE chunk_index = 0").fetch_one(&w.pool).await.unwrap();
    let fb = w.post(&w.alice.token, &format!("/memories/{id}/feedback"), json!({"verdict": "wrong"})).await;
    assert_status(&fb, StatusCode::UNPROCESSABLE_ENTITY, "invalid_request");

    let gone = ok(&w.delete(&w.alice.token, &format!("/memories/{id}")).await);
    assert_eq!((gone["table"].as_str(), gone["action"].as_str()), (Some("documents"), Some("retired")));
    let retired: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT retired_at FROM documents WHERE id = $1").bind(id).fetch_one(&w.pool).await.unwrap();
    assert!(retired.is_some());
    let listed = ok(&w.get(&w.alice.token, "/sources").await);
    assert_eq!(listed["sources"][0]["chunks"].as_i64().unwrap(), live_chunks(&w, URI).await);
}

#[sqlx::test(migrations = "../../migrations")]
async fn posting_the_same_content_again_restores_a_forgotten_chunk(pool: PgPool) {
    let w = world(pool).await;
    let first = ok(&w.post(&w.alice.token, "/sources", body(URI, &adr("Use Postgres."), "h1")).await);
    let chunks = first["chunks"].as_i64().unwrap();
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM documents WHERE chunk_index = 1").fetch_one(&w.pool).await.unwrap();
    ok(&w.delete(&w.alice.token, &format!("/memories/{id}")).await);
    assert_eq!(live_chunks(&w, URI).await, chunks - 1);

    let again = w.post(&w.alice.token, "/sources", body(URI, &adr("Use Postgres."), "h1")).await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.text());
    let out = again.json();
    assert_eq!((out["status"].as_str(), out["chunks"].as_i64()), (Some("replaced"), Some(chunks)));
    assert_eq!(live_chunks(&w, URI).await, chunks);

    let third = w.post(&w.alice.token, "/sources", body(URI, &adr("Use Postgres."), "h1")).await;
    assert_eq!(
        (third.status, third.json()["status"].as_str().map(String::from)),
        (StatusCode::OK, Some("unchanged".into()))
    );
}
