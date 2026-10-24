//! Spec 4.7 / 16.1: a second user's identity sees zero rows in every space-scoped table.

use std::collections::BTreeSet;

use memvault_core::auth::TokenKind;
use memvault_core::ids::new_id;
use memvault_core::schemas::common::MembershipRole;
use memvault_core::test_support::{seed_identity, seed_shared_space};
use memvault_core::{system_tx, user_tx};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

async fn space_scoped_tables(conn: &mut PgConnection) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT c.table_name::text FROM information_schema.columns c
           JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name
          WHERE c.table_schema = 'public' AND c.column_name = 'space_id' AND t.table_type = 'BASE TABLE'
          ORDER BY 1",
    )
    .fetch_all(conn)
    .await
    .unwrap()
}

macro_rules! exec {
    ($conn:expr, $sql:expr $(, $bind:expr)* $(,)?) => {
        sqlx::query($sql)$(.bind($bind))*.execute(&mut *$conn).await.unwrap_or_else(|e| panic!("{}: {e}", $sql))
    };
}

/// One row in every foundation space-scoped table. Returns the tables seeded.
async fn seed_space(conn: &mut PgConnection, space: Uuid, user: Uuid, client: Uuid) -> BTreeSet<&'static str> {
    let (sid, oid, eid, fid, wid) = (new_id(), new_id(), new_id(), new_id(), new_id());
    exec!(
        conn,
        "INSERT INTO sessions (id, space_id, client_id, harness, external_session_id, project_key)
                VALUES ($1, $2, $3, 'claude-code', $4, 'local/p')",
        sid,
        space,
        client,
        sid.to_string()
    );
    exec!(
        conn,
        "INSERT INTO events (id, space_id, session_id, seq, kind, payload, idempotency_key, occurred_at)
                VALUES ($1, $2, $3, 1, 'user_prompt', '{\"prompt\": \"hi\"}', $4, now())",
        new_id(),
        space,
        sid,
        format!("{sid}:1")
    );
    exec!(conn, "INSERT INTO observations (id, space_id, scope_kind, scope_key, session_id, type, title, extracted_by, content_hash)
                VALUES ($1, $2, 'project', 'local/p', $3, 'decision', 'secret plan', '{\"runner_kind\": \"explicit\"}', 'h')", oid, space, sid);
    exec!(
        conn,
        "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, session_id, next_steps, extracted_by)
                VALUES ($1, $2, 'project', 'local/p', 'session', $3, 'ship it', '{}')",
        new_id(),
        space,
        sid
    );
    exec!(
        conn,
        "INSERT INTO entities (id, space_id, kind, canonical_name) VALUES ($1, $2, 'user', 'alex')",
        eid,
        space
    );
    exec!(
        conn,
        "INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text, status)
                VALUES ($1, $2, 'global', '', $3, 'prefers.editor', '\"helix\"', 'helix', 'active')",
        fid,
        space,
        eid
    );
    exec!(
        conn,
        "INSERT INTO procedures (id, space_id, scope_kind, scope_key, name, task_class, trigger_text, body_md)
                VALUES ($1, $2, 'project', 'local/p', 'Deploy', 'deploy', 'when deploying', 'run migrations')",
        new_id(),
        space
    );
    exec!(
        conn,
        "INSERT INTO work_state (id, space_id, scope_kind, scope_key, list_name, fields)
                VALUES ($1, $2, 'project', 'local/p', 'todo', '{\"a\": \"done\"}')",
        new_id(),
        space
    );
    exec!(
        conn,
        "INSERT INTO profiles (space_id, scope_kind, scope_key, body_md, token_estimate, facts_version)
                VALUES ($1, 'global', '', '- helix', 2, 1)",
        space
    );
    exec!(
        conn,
        "INSERT INTO embeddings (space_id, owner_table, owner_id, part, model, embedding, scope_kind, scope_key)
                VALUES ($1, 'observations', $2, 'title', 'bge-m3', $3, 'project', 'local/p')",
        space,
        oid,
        pgvector::Vector::from(vec![0.1f32; 1024])
    );
    exec!(conn, "INSERT INTO work_packets (id, space_id, kind, session_id, prompt_ref, model_hint, inputs, output_schema, idempotency_key)
                VALUES ($1, $2, 'extract', $3, 'observer.code@v3', 'sonnet', '{}', 'observer.v1', $4)", wid, space, sid, format!("extract:{sid}:1"));
    exec!(
        conn,
        "INSERT INTO memory_audit (id, space_id, actor, action, target_table, target_id)
                VALUES ($1, $2, 'planner', 'fact.create', 'facts', $3)",
        new_id(),
        space,
        fid
    );
    exec!(
        conn,
        "INSERT INTO retrieval_log (id, space_id, client_id, mode, scopes, candidates, selected, latency_ms, reranked)
                VALUES ($1, $2, $3, 'push', '[]', '[]', '[]', 5, false)",
        new_id(),
        space,
        client
    );
    exec!(
        conn,
        "INSERT INTO access_log (id, space_id, client_id, action, target_ids) VALUES ($1, $2, $3, 'get', $4)",
        new_id(),
        space,
        client,
        vec![oid]
    );
    exec!(
        conn,
        "INSERT INTO procedure_hints (id, space_id, scope_kind, scope_key, task_class, note, observation_id)
                VALUES ($1, $2, 'project', 'local/p', 'deploy', 'run migrations first', $3)",
        new_id(),
        space,
        oid
    );
    exec!(
        conn,
        "INSERT INTO context_cache (cache_key, user_id, space_id, space_ids, scopes, body_md, etag)
                VALUES ($1, $2, $3, $4, $5, '## Memory', 'e1')",
        format!("k:{space}"),
        user,
        space,
        vec![space],
        vec!["global:", "project:local/p"]
    );
    BTreeSet::from([
        "sessions",
        "events",
        "observations",
        "summaries",
        "entities",
        "facts",
        "procedures",
        "work_state",
        "profiles",
        "embeddings",
        "work_packets",
        "memory_audit",
        "retrieval_log",
        "access_log",
        "procedure_hints",
        "context_cache",
    ])
}

async fn count(conn: &mut PgConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(conn).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_space_table_has_forced_rls(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tables = space_scoped_tables(&mut conn).await;
    assert!(tables.iter().any(|t| t == "embeddings") && tables.iter().any(|t| t == "observations"));
    for t in &tables {
        let (rls, forced): (bool, bool) =
            sqlx::query_as("SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE oid = $1::regclass")
                .bind(t)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert!(rls && forced, "{t}: RLS not enabled and forced");
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_policies WHERE schemaname = 'public' AND tablename = $1 AND 'memsvc_app' = ANY (roles)",
        )
        .bind(t)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert!(n >= 1, "{t}: no policy for memsvc_app");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn second_user_sees_nothing(pool: PgPool) {
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let bob = seed_identity(&pool, TokenKind::Device).await;
    let mut tx = system_tx(&pool).await.unwrap();
    let seeded = seed_space(&mut tx, alice.space_id, alice.user_id, alice.client_id).await;
    let tables = space_scoped_tables(&mut tx).await;
    tx.commit().await.unwrap();
    let missing: Vec<_> = seeded.iter().filter(|t| !tables.iter().any(|x| x == *t)).collect();
    assert!(missing.is_empty(), "{missing:?}");

    let mut tx = user_tx(&pool, &alice.identity).await.unwrap();
    for t in &seeded {
        let n = count(&mut tx, &format!("SELECT count(*) FROM {t} WHERE space_id = '{}'", alice.space_id)).await;
        assert!(n >= 1, "owner cannot see own rows in {t}");
    }
    drop(tx);

    let mut tx = user_tx(&pool, &bob.identity).await.unwrap();
    let user: String = sqlx::query_scalar("SELECT current_user::text").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(user, "memsvc_app");
    for t in &tables {
        // bob sees exactly his own membership rows
        let sql = if t == "memberships" {
            format!("SELECT count(*) FROM memberships WHERE user_id <> '{}'", bob.user_id)
        } else {
            format!("SELECT count(*) FROM {t}")
        };
        assert_eq!(count(&mut tx, &sql).await, 0, "{t} leaks rows to another user");
    }
    // the vector leg itself is filtered by the policy
    let near: Vec<Uuid> = sqlx::query_scalar("SELECT owner_id FROM embeddings ORDER BY embedding <=> $1 LIMIT 5")
        .bind(pgvector::Vector::from(vec![0.1f32; 1024]))
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert!(near.is_empty());
    // updates and deletes silently match nothing
    let n = sqlx::query("UPDATE observations SET title = 'x'").execute(&mut *tx).await.unwrap().rows_affected();
    assert_eq!(n, 0);
    let n = sqlx::query("DELETE FROM facts").execute(&mut *tx).await.unwrap().rows_affected();
    assert_eq!(n, 0);
    drop(tx);

    // no app.user_id at all: nothing visible either
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql("SET LOCAL ROLE memsvc_app").execute(&mut *tx).await.unwrap();
    assert_eq!(count(&mut tx, "SELECT count(*) FROM observations").await, 0);
    assert_eq!(count(&mut tx, "SELECT count(*) FROM embeddings").await, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn second_user_cannot_write_into_foreign_space(pool: PgPool) {
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let bob = seed_identity(&pool, TokenKind::Device).await;
    let mut tx = user_tx(&pool, &bob.identity).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO observations (id, space_id, scope_kind, scope_key, type, title, extracted_by, content_hash)
         VALUES ($1, $2, 'global', '', 'change', 't', '{}', 'h')",
    )
    .bind(new_id())
    .bind(alice.space_id)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(err.as_database_error().and_then(|e| e.code()).as_deref(), Some("42501"));
    assert_eq!(memvault_core::ApiError::from(err).status, axum::http::StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../../migrations")]
async fn shared_space_reader(pool: PgPool) {
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let carol = seed_identity(&pool, TokenKind::Device).await;
    let shared = seed_shared_space(&pool, "team", alice.user_id, &[(carol.user_id, MembershipRole::Reader)]).await;
    let mut tx = system_tx(&pool).await.unwrap();
    seed_space(&mut tx, shared, alice.user_id, alice.client_id).await;
    tx.commit().await.unwrap();

    let mut tx = user_tx(&pool, &carol.identity).await.unwrap();
    assert_eq!(count(&mut tx, &format!("SELECT count(*) FROM observations WHERE space_id = '{shared}'")).await, 1);
    assert_eq!(count(&mut tx, &format!("SELECT count(*) FROM embeddings WHERE space_id = '{shared}'")).await, 1);
    // readers may append to logs ...
    sqlx::query(
        "INSERT INTO access_log (id, space_id, client_id, action, target_ids) VALUES ($1, $2, $3, 'get', '{}')",
    )
    .bind(new_id())
    .bind(shared)
    .bind(carol.client_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    // ... but context_cache rows are per user, and content is read-only
    assert_eq!(count(&mut tx, "SELECT count(*) FROM context_cache").await, 0);
    let n = sqlx::query("UPDATE observations SET title = 'x'").execute(&mut *tx).await.unwrap().rows_affected();
    assert_eq!(n, 0);
    tx.commit().await.unwrap();

    let mut tx = user_tx(&pool, &carol.identity).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO work_state (id, space_id, scope_kind, scope_key, list_name, fields) VALUES ($1, $2, 'global', '', 'todo', '{}')",
    )
    .bind(new_id())
    .bind(shared)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(err.as_database_error().and_then(|e| e.code()).as_deref(), Some("42501"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn invalidate_function_crosses_users(pool: PgPool) {
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let mut tx = system_tx(&pool).await.unwrap();
    seed_space(&mut tx, alice.space_id, alice.user_id, alice.client_id).await;
    tx.commit().await.unwrap();
    // runs as memsvc_app with no user context, like the API's LISTEN loop
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql("SET LOCAL ROLE memsvc_app").execute(&mut *tx).await.unwrap();
    let sql = "SELECT memsvc_invalidate_context($1, $2, $3)";
    let n: i32 =
        sqlx::query_scalar(sql).bind(alice.space_id).bind("project").bind("other").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(n, 0);
    let n: i32 =
        sqlx::query_scalar(sql).bind(alice.space_id).bind("project").bind("local/p").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(n, 1);
}
