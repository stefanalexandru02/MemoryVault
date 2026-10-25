use std::collections::BTreeSet;

use memvault_core::auth::TokenKind;
use memvault_core::db::MIGRATOR;
use memvault_core::test_support::seed_identity;
use memvault_core::{ids::new_id, system_tx};
use sqlx::PgPool;

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
