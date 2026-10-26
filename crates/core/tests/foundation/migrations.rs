use std::collections::BTreeSet;
use std::time::Duration;

use memvault_core::auth::TokenKind;
use memvault_core::db::{MIGRATOR, applied_migrations, migrate, wait_for_schema};
use memvault_core::test_support::seed_identity;
use memvault_core::{ids::new_id, system_tx};
use sqlx::PgPool;
use sqlx::migrate::Migrator;

const EXPECTED_TABLES: &[&str] = &[
    "users",
    "spaces",
    "memberships",
    "clients",
    "api_tokens",
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
    "jobs",
    "memory_audit",
    "retrieval_log",
    "access_log",
    "procedure_hints",
    "context_cache",
    "rate_limits",
    "_sqlx_migrations",
];

async fn tables(pool: &PgPool) -> BTreeSet<String> {
    sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname = 'public'")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .collect()
}

#[sqlx::test(migrations = "../../migrations")]
async fn rerun_is_noop(pool: PgPool) {
    MIGRATOR.run(&pool).await.unwrap();
    let applied: Vec<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
        .fetch_all(&pool)
        .await
        .unwrap();
    let known: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    assert_eq!(applied, known);
    assert!(known.contains(&99));
}

#[sqlx::test(migrations = false)]
async fn fresh_database_twice(pool: PgPool) {
    MIGRATOR.run(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let found = tables(&pool).await;
    for t in EXPECTED_TABLES {
        assert!(found.contains(*t), "missing table {t}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn schema_details(pool: PgPool) {
    let cols: BTreeSet<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns WHERE table_schema = 'public'",
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .collect();
    assert!(cols.contains(&("clients".into(), "current_project_key".into())));
    assert!(cols.contains(&("events".into(), "payload_trimmed".into())));
    let indexes: BTreeSet<String> =
        sqlx::query_scalar("SELECT indexname::text FROM pg_indexes WHERE schemaname = 'public'")
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert!(indexes.contains("emb_hnsw") && indexes.contains("facts_one_active"));
    let pg_search: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_search')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(indexes.contains("obs_bm25"), pg_search);
    let roles: Vec<(String, bool)> =
        sqlx::query_as("SELECT rolname::text, rolbypassrls FROM pg_roles WHERE rolname LIKE 'memsvc_%' ORDER BY 1")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(roles, vec![("memsvc_app".into(), false), ("memsvc_system".into(), true)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn grants_cover_every_table_and_later_ones(pool: PgPool) {
    // Every table (including module migrations 0100+) is usable by both roles (except the few that
    // 0900_hardening narrows for the app role) ...
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c
          WHERE c.relnamespace = 'public'::regnamespace AND c.relkind = 'r' AND c.relname <> '_sqlx_migrations'
            AND c.relname NOT IN ('users', 'spaces', 'api_tokens', 'clients', 'jobs')
            AND NOT (has_table_privilege('memsvc_app', c.oid, 'SELECT,INSERT,UPDATE,DELETE')
                 AND has_table_privilege('memsvc_system', c.oid, 'SELECT,INSERT,UPDATE,DELETE'))",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(missing.is_empty(), "tables without grants: {missing:?}");
    // ... and a table created after the migrations (as a later migration would) is covered by
    // the default privileges of 0099.
    sqlx::raw_sql("CREATE TABLE later_module_table (id bigserial PRIMARY KEY, space_id uuid NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let ok: (bool, bool) = sqlx::query_as(
        "SELECT has_table_privilege('memsvc_app', 'later_module_table', 'SELECT,INSERT,UPDATE,DELETE'),
                has_sequence_privilege('memsvc_system', 'later_module_table_id_seq', 'USAGE')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ok, (true, true));
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_app_role_cannot_rewrite_tenancy_or_tokens(pool: PgPool) {
    let can = |privilege: &'static str, table: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT has_table_privilege('memsvc_app', $1, $2)")
                .bind(table)
                .bind(privilege)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    for table in ["users", "spaces", "api_tokens", "clients"] {
        assert!(can("SELECT", table).await, "{table} is read for authentication");
        for privilege in ["INSERT", "DELETE"] {
            assert!(!can(privilege, table).await, "{privilege} on {table}");
        }
    }
    for table in ["users", "spaces"] {
        assert!(!can("UPDATE", table).await, "UPDATE on {table}");
    }
    // only the bookkeeping columns stay writable
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname::text FROM pg_attribute a
          WHERE a.attrelid = 'api_tokens'::regclass AND a.attnum > 0 AND NOT a.attisdropped
            AND has_column_privilege('memsvc_app', a.attrelid, a.attnum, 'UPDATE')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(columns, ["last_used_at"]);
    // the cross-tenant functions are not callable by other roles
    let public: bool = sqlx::query_scalar(
        "SELECT has_function_privilege('public', 'memsvc_recall_invalidate(uuid, text, text)', 'EXECUTE')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!public);
}

#[sqlx::test(migrations = "../../migrations")]
async fn generated_search_columns(pool: PgPool) {
    let seeded = seed_identity(&pool, TokenKind::Device).await;
    let mut tx = system_tx(&pool).await.unwrap();
    let oid = new_id();
    sqlx::query(
        "INSERT INTO observations (id, space_id, scope_kind, scope_key, type, title, narrative, facts, extracted_by, content_hash)
         VALUES ($1, $2, 'project', 'p', 'decision', 'Chose pgvector', 'over Chroma', $3, $4, 'h')",
    )
    .bind(oid)
    .bind(seeded.space_id)
    .bind(vec!["one store", "RLS covers vectors"])
    .bind(serde_json::json!({"runner_kind": "explicit"}))
    .execute(&mut *tx)
    .await
    .unwrap();
    let (text, hit): (String, bool) = sqlx::query_as(
        "SELECT search_text, search_tsv @@ websearch_to_tsquery('simple', 'chroma vectors') FROM observations WHERE id = $1",
    )
    .bind(oid)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(text, "Chose pgvector  over Chroma one store RLS covers vectors");
    assert!(hit);
}

/// What `serve` runs at start-up: every migration once, however many processes start together.
#[sqlx::test(migrations = false)]
async fn startup_migration_applies_everything_once_even_when_replicas_race(pool: PgPool) {
    let total = MIGRATOR.iter().count() as i64;
    assert_eq!(applied_migrations(&pool).await, 0);

    let (a, b, c) = tokio::join!(migrate(&pool), migrate(&pool), migrate(&pool));
    for result in [a, b, c] {
        result.unwrap();
    }
    // One row per migration: a double run would have failed on the primary key or left duplicates.
    assert_eq!(applied_migrations(&pool).await, total);
    let rows: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT version) FROM _sqlx_migrations").fetch_one(&pool).await.unwrap();
    assert_eq!(rows, total);

    migrate(&pool).await.expect("a second start-up finds nothing to do");
    assert_eq!(applied_migrations(&pool).await, total);
}

/// A rollback to an older image must still start against a database a newer image migrated.
#[sqlx::test(migrations = "../../migrations")]
async fn startup_migration_tolerates_a_newer_database(pool: PgPool) {
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (99999999999999, 'from the future', true, '\\x00', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    migrate(&pool).await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn the_worker_waits_for_the_schema(pool: PgPool) {
    let err = wait_for_schema(&pool, Duration::from_millis(300)).await.unwrap_err().to_string();
    assert!(err.contains("0 of"), "{err}");

    // Another process (the API) migrates while the worker waits.
    let migrating = pool.clone();
    let api = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        migrate(&migrating).await.unwrap();
    });
    wait_for_schema(&pool, Duration::from_secs(20)).await.unwrap();
    api.await.unwrap();
    wait_for_schema(&pool, Duration::ZERO).await.expect("an up-to-date schema needs no waiting");
}

/// Migration 0901 estimates the work behind rows that existed before it, from the events that are still there.
#[sqlx::test(migrations = false)]
async fn work_tokens_are_backfilled_from_the_remaining_events(pool: PgPool) {
    let before = MIGRATOR.iter().filter(|m| m.version < 901).cloned().collect::<Vec<_>>();
    Migrator { migrations: before.into(), ..Migrator::DEFAULT }.run(&pool).await.unwrap();
    let seeded = seed_identity(&pool, TokenKind::Device).await;
    let space = seeded.space_id;
    let session = new_id();
    sqlx::query(
        "INSERT INTO sessions (id, space_id, client_id, harness, external_session_id) VALUES ($1, $2, $3, 'claude-code', 's')",
    )
    .bind(session)
    .bind(space)
    .bind(seeded.client_id)
    .execute(&pool)
    .await
    .unwrap();
    for (window, prompt) in [(1, 1), (2, 2)] {
        sqlx::query(
            "INSERT INTO session_windows (space_id, session_id, window_no, prompt_no, closed_at)
             VALUES ($1, $2, $3, $4, now())",
        )
        .bind(space)
        .bind(session)
        .bind(window)
        .bind(prompt)
        .execute(&pool)
        .await
        .unwrap();
    }
    // Two events of the first prompt's window, one of the second whose payload retention has trimmed.
    let mut events = Vec::new();
    for (seq, window, tokens, trimmed) in [(1, 1, 10, false), (2, 1, 20, false), (3, 2, 40, true)] {
        let id = new_id();
        sqlx::query(
            "INSERT INTO events (id, space_id, session_id, seq, kind, payload, payload_trimmed, idempotency_key,
                                 occurred_at, token_estimate, window_no)
             VALUES ($1, $2, $3, $4, 'tool_use', '{}', $5, $1::text, now(), $6, $7)",
        )
        .bind(id)
        .bind(space)
        .bind(session)
        .bind(seq)
        .bind(trimmed)
        .bind(tokens)
        .bind(window)
        .execute(&pool)
        .await
        .unwrap();
        events.push(id);
    }
    let obs = |sources: Vec<uuid::Uuid>| {
        let pool = pool.clone();
        async move {
            let id = new_id();
            sqlx::query(
                "INSERT INTO observations (id, space_id, scope_kind, scope_key, session_id, type, title, extracted_by,
                                           content_hash, source_event_ids)
                 VALUES ($1, $2, 'project', 'p', $3, 'bugfix', 't', '{}', $1::text, $4)",
            )
            .bind(id)
            .bind(space)
            .bind(session)
            .bind(sources)
            .execute(&pool)
            .await
            .unwrap();
            id
        }
    };
    let cited = obs(vec![events[0], events[1], events[1]]).await;
    let gone = obs(vec![events[0], new_id()]).await;
    let uncited = obs(Vec::new()).await;
    let summary = |grain: &'static str, prompt_no: Option<i32>, session: Option<uuid::Uuid>| {
        let pool = pool.clone();
        async move {
            let id = new_id();
            sqlx::query(
                "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, session_id, prompt_no, extracted_by)
                 VALUES ($1, $2, 'project', 'p', $3, $4, $5, '{}')",
            )
            .bind(id)
            .bind(space)
            .bind(grain)
            .bind(session)
            .bind(prompt_no)
            .execute(&pool)
            .await
            .unwrap();
            id
        }
    };
    let first_prompt = summary("prompt", Some(1), Some(session)).await;
    let second_prompt = summary("prompt", Some(2), Some(session)).await;
    let whole = summary("session", None, Some(session)).await;
    let day = summary("day", None, None).await;
    let subject = new_id();
    sqlx::query("INSERT INTO entities (id, space_id, kind, canonical_name) VALUES ($1, $2, 'user', 'alex')")
        .bind(subject)
        .bind(space)
        .execute(&pool)
        .await
        .unwrap();
    let fact = |predicate: &'static str, evidence: Vec<uuid::Uuid>| {
        let pool = pool.clone();
        async move {
            let id = new_id();
            sqlx::query(
                "INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text,
                                    evidence_observation_ids)
                 VALUES ($1, $2, 'global', '', $3, $4, '\"x\"', 'x', $5)",
            )
            .bind(id)
            .bind(space)
            .bind(subject)
            .bind(predicate)
            .bind(evidence)
            .execute(&pool)
            .await
            .unwrap();
            id
        }
    };
    let derived = fact("a.b", vec![cited, gone]).await;
    let by_hand = fact("c.d", Vec::new()).await;
    let procedure = new_id();
    sqlx::query(
        "INSERT INTO procedures (id, space_id, scope_kind, scope_key, name, task_class, trigger_text, body_md,
                                 source_observation_ids)
         VALUES ($1, $2, 'project', 'p', 'n', 'deploy', 't', 'b', $3)",
    )
    .bind(procedure)
    .bind(space)
    .bind(vec![cited])
    .execute(&pool)
    .await
    .unwrap();

    MIGRATOR.run(&pool).await.unwrap();
    let work = |table: &'static str, id: uuid::Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<i32>>(&format!("SELECT work_tokens FROM {table} WHERE id = $1"))
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(work("observations", cited).await, Some(30), "each cited event once");
    assert_eq!(work("observations", gone).await, None, "an event is gone");
    assert_eq!(work("observations", uncited).await, None);
    assert_eq!(work("summaries", first_prompt).await, Some(30), "the windows of its prompt");
    assert_eq!(work("summaries", second_prompt).await, Some(40), "a trimmed payload keeps its estimate");
    assert_eq!(work("summaries", whole).await, Some(70), "every event of the session");
    assert_eq!(work("summaries", day).await, None);
    assert_eq!(work("facts", derived).await, Some(30), "the evidence that has an estimate");
    assert_eq!(work("facts", by_hand).await, None);
    assert_eq!(work("procedures", procedure).await, Some(30));

    // From now on the trigger keeps facts and procedures in step with their evidence.
    sqlx::query("UPDATE facts SET evidence_observation_ids = '{}' WHERE id = $1")
        .bind(derived)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(work("facts", derived).await, None);
    let new_fact = fact("e.f", vec![cited]).await;
    assert_eq!(work("facts", new_fact).await, Some(30));
}
