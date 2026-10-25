//! Ingested documents: recall with a source, search by type, ranking against facts, never in the SessionStart
//! block, retirement and point-in-time search, row security.

use memvault_core::auth::scopes::TokenKind;
use memvault_core::schemas::common::{InjectionMode, MemoryTable, Scope};
use memvault_core::schemas::memories::{MemoryItem, SearchQuery};
use memvault_core::schemas::recall::{Budgets, RecallRequest};
use memvault_core::test_support::seed_identity;
use memvault_recall::{context, engine, render};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::*;

const URI: &str = "docs/adr/0003-postgres.md";
const ADR: &str = "We chose Postgres with pgvector for storage because one database keeps the backups simple";

fn req(query: &str) -> RecallRequest {
    RecallRequest {
        query: Some(query.into()),
        scopes: Some(vec![Scope::global(), project()]),
        ..RecallRequest::default()
    }
}

async fn adr(w: &World) -> Uuid {
    insert_document(w, w.alice.space_id, &project(), URI, &["ADR 3", "Decision"], ADR).await
}

fn search(q: &str, types: &[&str]) -> SearchQuery {
    SearchQuery {
        q: Some(q.into()),
        types: Some(types.iter().map(|t| t.to_string()).collect()),
        limit: 20,
        ..SearchQuery::default()
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn recall_shows_a_document_with_its_source(pool: PgPool) {
    let w = world(pool).await;
    let id = adr(&w).await;
    let resp = engine::recall(&w.state, &w.alice.identity, req("why postgres pgvector storage")).await.unwrap();
    let doc = resp.slots.documents.first().expect("the chunk is recalled");
    assert_eq!(doc.item.id, id);
    assert_eq!(doc.source, "docs/adr/0003-postgres.md#ADR 3 > Decision");
    assert_eq!((doc.uri.as_str(), doc.chunk_index), (URI, 0));
    assert_eq!(doc.content, ADR);
    let md = resp.rendered_markdown.unwrap();
    assert!(md.contains("### Documents\n- We chose Postgres"), "{md}");
    assert!(md.contains("source: docs/adr/0003-postgres.md#ADR 3 > Decision"), "{md}");
    assert!(resp.tokens.by_slot["documents"] > 0);

    // A budget of zero switches the slot off; a chunk above the first heading names just the file.
    let off = RecallRequest { budgets: Budgets { documents: 0, ..Budgets::default() }, ..req("postgres") };
    assert!(engine::recall(&w.state, &w.alice.identity, off).await.unwrap().slots.documents.is_empty());
    insert_document(&w, w.alice.space_id, &project(), "README.md", &[], "Run make build to compile the sandbox").await;
    let resp = engine::recall(&w.state, &w.alice.identity, req("make build sandbox")).await.unwrap();
    assert_eq!(resp.slots.documents[0].source, "README.md");
}

#[sqlx::test(migrations = "../../migrations")]
async fn documents_never_reach_the_session_start_block(pool: PgPool) {
    let w = world(pool).await;
    adr(&w).await;
    let md = context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", None).await.unwrap();
    assert!(!md.contains("Postgres") && !md.contains(URI) && !md.contains("Documents"), "{md}");
    let lean =
        context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", Some(InjectionMode::Lean))
            .await
            .unwrap();
    assert!(!lean.contains(URI), "{lean}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_document_ranks_below_a_fact_of_equal_relevance(pool: PgPool) {
    let w = world(pool).await;
    let sentence = "restart the nginx proxy after the certificate renewal";
    let space = w.alice.space_id;
    insert_fact(&w, Fact::new(space, "deploy.runbook", sentence)).await;
    insert_document(&w, space, &project(), "docs/runbook.md", &["Runbook"], sentence).await;

    for rerank in [true, false] {
        // No profile: the fallback profile would take the fact into its own slot.
        let budgets = Budgets { profile: 0, ..Budgets::default() };
        let r = RecallRequest { rerank, budgets, ..req("restart nginx proxy certificate renewal") };
        let resp = engine::recall(&w.state, &w.alice.identity, r).await.unwrap();
        let (fact, doc) = (&resp.slots.facts[0].item, &resp.slots.documents[0].item);
        assert!(fact.score > doc.score, "rerank={rerank}: fact {} vs document {}", fact.score, doc.score);
    }

    let q = SearchQuery {
        tables: Some(vec![MemoryTable::Documents, MemoryTable::Facts]),
        types: None,
        ..search("restart nginx proxy certificate renewal", &[])
    };
    let rows = engine::search(&w.state, &w.alice.identity, q).await.unwrap().rows;
    let tables: Vec<MemoryTable> = rows.iter().map(|r| r.table).collect();
    assert_eq!(tables, [MemoryTable::Facts, MemoryTable::Documents]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn search_finds_documents_by_type(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let id = adr(&w).await;
    insert_obs(&w, Obs::new(space, "Chose postgres for storage", "postgres pgvector storage decision")).await;

    let rows = engine::search(&w.state, &w.alice.identity, search("postgres pgvector storage", &["document"]))
        .await
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].id, rows[0].table), (id, MemoryTable::Documents));
    assert_eq!(rows[0].row_type.as_deref(), Some("document"));
    assert_eq!(rows[0].title, "docs/adr/0003-postgres.md#ADR 3 > Decision");
    assert!(rows[0].score.is_some_and(|s| s > 0.0));

    // Observation types and the document type mix; without the type, search stays on observations and summaries.
    let mixed =
        engine::search(&w.state, &w.alice.identity, search("postgres pgvector storage", &["discovery", "document"]))
            .await
            .unwrap()
            .rows;
    assert_eq!(mixed.len(), 2, "{mixed:?}");
    let plain = SearchQuery { types: None, ..search("postgres pgvector storage", &[]) };
    let plain = engine::search(&w.state, &w.alice.identity, plain).await.unwrap().rows;
    assert!(plain.iter().all(|r| r.table != MemoryTable::Documents), "{plain:?}");
    // No query: the newest chunks.
    let recent = SearchQuery { q: None, ..search("", &["document"]) };
    assert_eq!(engine::search(&w.state, &w.alice.identity, recent).await.unwrap().rows.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn retired_chunks_are_hidden_but_not_from_a_past_search(pool: PgPool) {
    let w = world(pool).await;
    let id = adr(&w).await;
    sqlx::query(
        "UPDATE documents SET created_at = '2026-01-01T00:00:00Z', retired_at = '2026-02-01T00:00:00Z' WHERE id = $1",
    )
    .bind(id)
    .execute(&w.pool)
    .await
    .unwrap();
    let found = |as_of: Option<&str>| {
        let q = SearchQuery { as_of: as_of.map(|t| t.parse().unwrap()), ..search("postgres pgvector", &["document"]) };
        let state = w.state.clone();
        let who = w.alice.identity.clone();
        async move { engine::search(&state, &who, q).await.unwrap().rows.len() }
    };
    assert_eq!(found(None).await, 0);
    assert_eq!(found(Some("2026-01-15T00:00:00Z")).await, 1);
    assert_eq!(found(Some("2026-03-01T00:00:00Z")).await, 0);
    assert!(
        engine::recall(&w.state, &w.alice.identity, req("postgres pgvector")).await.unwrap().slots.documents.is_empty()
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn get_and_explain_show_the_source(pool: PgPool) {
    let w = world(pool).await;
    let id = adr(&w).await;
    let got = engine::get_memories(&w.state, &w.alice.identity, &[id]).await.unwrap();
    let MemoryItem::Documents(doc) = &got.items[0] else { panic!("{:?}", got.items) };
    assert_eq!((doc.source_uri.as_str(), doc.content.as_str()), (URI, ADR));
    let md = render::memory_items_markdown(&got.items);
    assert!(md.contains("source: docs/adr/0003-postgres.md#ADR 3 > Decision") && md.ends_with(ADR), "{md}");

    let explained = engine::explain(&w.state, &w.alice.identity, id).await.unwrap();
    assert_eq!(explained.table, MemoryTable::Documents);
    let by = explained.provenance.unwrap().extracted_by;
    assert_eq!((by["source_uri"].as_str(), by["chunk_index"].as_i64()), (Some(URI), Some(0)));

    let refs =
        engine::resolve_refs(&w.state, &w.alice.identity, &[format!("doc#{}", memvault_core::ids::short_id(&id))])
            .await
            .unwrap();
    assert_eq!(refs[0].1, engine::RefMatch::One(id));
    let history = engine::history(&w.state, &w.alice.identity, id).await.unwrap();
    assert_eq!((history.entries.len(), history.entries[0].status.as_str()), (1, "active"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn another_user_sees_no_documents(pool: PgPool) {
    let w = world(pool.clone()).await;
    let id = adr(&w).await;
    let bob = seed_identity(&pool, TokenKind::Device).await;
    let rows = engine::search(&w.state, &bob.identity, search("postgres pgvector", &["document"])).await.unwrap().rows;
    assert!(rows.is_empty());
    let resp = engine::recall(&w.state, &bob.identity, req("postgres pgvector")).await.unwrap();
    assert!(resp.slots.documents.is_empty());
    assert_eq!(engine::get_memories(&w.state, &bob.identity, &[id]).await.unwrap().missing, vec![id]);
}
