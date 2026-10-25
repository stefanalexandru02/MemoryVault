//! SessionStart block, context cache, invalidation listener, worker re-render.

use std::time::Duration;

use memvault_core::auth::admin::ensure_client;
use memvault_core::ids::short_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::common::{ClientKind, Scope, ScopeKind, Sensitivity, SummaryGrain};
use memvault_core::system_tx;
use memvault_recall::{cache, context, render};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::common::*;

struct Seeded {
    decision: Uuid,
    bugfix: Uuid,
    subagent: Uuid,
    secret: Uuid,
    desktop: Uuid,
}

async fn seed(w: &World) -> Seeded {
    let space = w.alice.space_id;
    let mut tx = system_tx(&w.pool).await.unwrap();
    let desktop = ensure_client(&mut tx, w.alice.user_id, "desktop", ClientKind::ClaudeCode, None, None).await.unwrap();
    tx.commit().await.unwrap();
    // This device: a previous visit 2 days ago and the session asking for context (1 minute ago).
    insert_session(&w.pool, space, w.alice.client_id, PROJECT, 2.0 * 24.0 * 60.0).await;
    insert_session(&w.pool, space, w.alice.client_id, PROJECT, 1.0).await;
    // Elsewhere since then: the desktop, with a session summary.
    let elsewhere = insert_session(&w.pool, space, desktop, PROJECT, 24.0 * 60.0).await;
    insert_summary(
        w,
        space,
        &project(),
        Some(elsewhere),
        SummaryGrain::Session,
        "Add rerank stage to recall",
        "wire TEI reranker container",
        1.0,
    )
    .await;
    // Before the last visit: not shown.
    let older = insert_session(&w.pool, space, desktop, PROJECT, 5.0 * 24.0 * 60.0).await;
    insert_summary(w, space, &project(), Some(older), SummaryGrain::Session, "Old work", "old next", 5.0).await;

    let mut d = Obs::new(space, "Chose pgvector over Chroma for the server", "one database");
    d.obs_type = "decision";
    let decision = insert_obs(w, d).await;
    let mut b = Obs::new(space, "Fixed off-by-one in window close", "seq counting");
    b.obs_type = "bugfix";
    b.age_days = 0.5;
    let bugfix = insert_obs(w, b).await;
    let mut s = Obs::new(space, "Subagent explored the repo", "files listed");
    s.entities = vec!["subagent:Explore".into()];
    let subagent = insert_obs(w, s).await;
    let mut x = Obs::new(space, "Root password is hunter2", "secret");
    x.sensitivity = Sensitivity::Secret;
    let secret = insert_obs(w, x).await;

    let mut pinned = Fact::new(space, "prefers.vector_store", "pgvector for personal projects");
    pinned.pinned = true;
    insert_fact(w, pinned).await;
    insert_procedure(w, space, "Deploy memory-api", "deploy", "run migrations Job first, then roll Deployment").await;
    insert_work_state(&w.pool, space, &project(), "todo", json!({"migrations": "done", "api deploy": "todo"})).await;
    Seeded { decision, bugfix, subagent, secret, desktop }
}

#[sqlx::test(migrations = "../../migrations")]
async fn context_block_has_every_section(pool: PgPool) {
    let w = world(pool).await;
    let s = seed(&w).await;
    let md = context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup").await.unwrap();

    assert!(
        md.starts_with("## Memory\n### Profile\n- prefers.vector_store: pgvector for personal projects [fact#"),
        "{md}"
    );
    let pos = |needle: &str| md.find(needle).unwrap_or_else(|| panic!("missing {needle:?} in\n{md}"));
    let since = pos(&format!("### Since you were here ({}, last ", w.alice.device_id));
    assert!(pos("### Profile") < since && since < pos("### Open work"));
    assert!(pos("### Open work") < pos("### Relevant procedures"));
    assert!(pos("### Relevant procedures") < pos(&format!("### {}", render::RECENT_OBSERVATIONS_TITLE)));
    assert!(md.contains("desktop · \"Add rerank stage to recall\" → next: wire TEI reranker container"), "{md}");
    let since_text = &md[since..pos("### Open work")];
    assert!(!since_text.contains("Old work"), "sessions before the last visit are not listed:\n{md}");
    assert!(md.contains("- todo/homelab: api deploy ☐ · migrations ✓"), "{md}");
    assert!(md.contains("- **Deploy memory-api** (v1): run migrations Job first"), "{md}");
    assert!(md.contains(render::INDEX_HEADER));
    assert!(md.contains(&render::legend()));
    // Index: newest first, glyphs, short ids; subagent and secret observations hidden.
    let b = pos(&format!("| {} |", short_id(&s.bugfix)));
    let d = pos(&format!("| {} |", short_id(&s.decision)));
    assert!(b < d, "newest first");
    assert!(md.contains("| ⚖ | Chose pgvector over Chroma for the server |"), "{md}");
    assert!(md.contains("| ● | Fixed off-by-one in window close |"), "{md}");
    assert!(!md.contains(&short_id(&s.subagent)) && !md.contains(&short_id(&s.secret)));
    assert!(!md.contains("hunter2"));
    // Recent sessions (session summaries' next steps) from the cached part.
    assert!(md.contains("### Recent sessions\n"), "{md}");

    // Cached: one row carrying what a re-render needs; a second call is identical.
    let (n, client, project_key): (i64, Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT count(*) OVER (), client_id, project_key FROM context_cache")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!((n, client, project_key.as_deref()), (1, Some(w.alice.client_id), Some(PROJECT)));
    let again = context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup").await.unwrap();
    assert_eq!(md, again);
    // Served from the cache, with since-you-were-here still computed live.
    sqlx::query("UPDATE context_cache SET body_md = '### Open work\n- CACHED'").execute(&w.pool).await.unwrap();
    let cached = context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup").await.unwrap();
    assert!(cached.contains("### Since you were here") && cached.ends_with("### Open work\n- CACHED"), "{cached}");
    // A stale row (older than the max age) is rendered again.
    sqlx::query("UPDATE context_cache SET rendered_at = now() - interval '2 hours'").execute(&w.pool).await.unwrap();
    let fresh = context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup").await.unwrap();
    assert_eq!(fresh, md);

    // Other sources are separate cache keys; resume returns nothing.
    context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "clear").await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM context_cache").fetch_one(&w.pool).await.unwrap();
    assert_eq!(rows, 2);
    assert_eq!(context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "resume").await.unwrap(), "");
    // The retrieval log records context renders (cache misses).
    let modes: Vec<String> = sqlx::query_scalar("SELECT mode FROM retrieval_log").fetch_all(&w.pool).await.unwrap();
    assert!(modes.iter().all(|m| m == "context") && modes.len() == 3, "{modes:?}");
    let _ = s.desktop;
}

#[sqlx::test(migrations = "../../migrations")]
async fn context_without_project_or_data(pool: PgPool) {
    let w = world(pool).await;
    let md = context::render_context(&w.state, &w.alice.identity, None, None, "startup").await.unwrap();
    assert_eq!(md, "## Memory\n_No memories for this project yet._");
    // Device-scoped rows show up under the device given in the query.
    let mut o = Obs::new(w.alice.space_id, "Laptop battery tweak", "tlp settings");
    o.scope = Scope::new(ScopeKind::Device, "laptop-x1");
    let id = insert_obs(&w, o).await;
    let md = context::render_context(&w.state, &w.alice.identity, None, Some("laptop-x1"), "startup").await.unwrap();
    assert!(md.contains(&short_id(&id)), "{md}");
    // Stale observer packets raise a banner.
    sqlx::query(
        "INSERT INTO work_packets (id, space_id, kind, prompt_ref, model_hint, inputs, output_schema, idempotency_key,
                                   created_at)
         VALUES ($1, $2, 'extract', 'observer.code@v1', 'sonnet', '{}', 'observer.v1', 'k', now() - interval '2 days')",
    )
    .bind(Uuid::now_v7())
    .bind(w.alice.space_id)
    .execute(&w.pool)
    .await
    .unwrap();
    let md = context::render_context(&w.state, &w.alice.identity, None, Some("laptop-x1"), "startup").await.unwrap();
    assert!(md.starts_with("## Memory\n> memory: 1 work packet(s) waiting since "), "{md}");
}

async fn cache_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM context_cache").fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn invalidation_deletes_matching_blocks_and_queues_rerender(pool: PgPool) {
    let w = world(pool).await;
    seed(&w).await;
    let space = w.alice.space_id;
    let first = context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup").await.unwrap();
    assert_eq!(cache_rows(&w.pool).await, 1);

    // Another project's scope or another space: nothing to delete.
    assert_eq!(cache::handle_payload(&w.state, &format!("{space}:project:github.com/x/y")).await.unwrap(), 0);
    assert_eq!(cache::handle_payload(&w.state, &format!("{}:project:{PROJECT}", Uuid::now_v7())).await.unwrap(), 0);
    assert_eq!(cache::handle_payload(&w.state, "garbage").await.unwrap(), 0);
    assert_eq!(cache_rows(&w.pool).await, 1);

    assert_eq!(cache::handle_payload(&w.state, &format!("{space}:project:{PROJECT}")).await.unwrap(), 1);
    assert_eq!(cache_rows(&w.pool).await, 0);
    let jobs: Vec<(Value, bool)> = sqlx::query_as(
        "SELECT payload, run_after > now() FROM jobs WHERE type = 'render_context' AND status = 'queued'",
    )
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert_eq!(jobs.len(), 1);
    let (payload, debounced) = &jobs[0];
    assert!(*debounced, "the re-render waits for the debounce");
    assert_eq!(payload["client_id"], json!(w.alice.client_id));
    assert_eq!(payload["project_key"], PROJECT);

    // A burst coalesces into the one queued job.
    context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup").await.unwrap();
    assert_eq!(cache::handle_payload(&w.state, &format!("{space}:global:")).await.unwrap(), 1);
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE type = 'render_context'").fetch_one(&w.pool).await.unwrap();
    assert_eq!(queued, 1);

    // The worker's re-render puts the block back; the next SessionStart is a cache hit.
    assert!(cache::rerender(&w.state, payload).await.unwrap());
    assert_eq!(cache_rows(&w.pool).await, 1);
    let body: String = sqlx::query_scalar("SELECT body_md FROM context_cache").fetch_one(&w.pool).await.unwrap();
    assert!(first.contains(body.lines().next().unwrap()));
    // A whole-space payload (bare id) also works; unknown clients are skipped by rerender.
    assert_eq!(cache::handle_payload(&w.state, &space.to_string()).await.unwrap(), 1);
    let gone = json!({"cache_key": "k", "user_id": Uuid::now_v7(), "client_id": Uuid::now_v7(), "source": "startup"});
    assert!(!cache::rerender(&w.state, &gone).await.unwrap());
}

#[sqlx::test(migrations = "../../migrations")]
async fn listener_deletes_blocks_on_notify(pool: PgPool) {
    let w = world(pool).await;
    seed(&w).await;
    let shutdown = CancellationToken::new();
    let handle = cache::spawn_listener(w.state.clone(), shutdown.clone());
    // Let the listener connect (it starts by dropping every cached block).
    tokio::time::sleep(Duration::from_millis(500)).await;
    context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup").await.unwrap();
    assert_eq!(cache_rows(&w.pool).await, 1);

    let mut tx = w.pool.begin().await.unwrap();
    notify_invalidate(&mut tx, w.alice.space_id, ScopeKind::Project, PROJECT).await.unwrap();
    tx.commit().await.unwrap();
    let mut deleted = false;
    for _ in 0..100 {
        if cache_rows(&w.pool).await == 0 {
            deleted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(deleted, "the listener deleted the block");
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle).await.expect("listener stops").unwrap();
}

/// A block whose render began before an invalidation committed must not be served, even though the
/// invalidation's DELETE could not see it (it was not committed yet).
#[sqlx::test(migrations = "../../migrations")]
async fn render_racing_with_invalidation_is_not_served(pool: PgPool) {
    let w = world(pool).await;
    let id = &w.alice.identity;
    let spaces = vec![w.alice.space_id];
    let scopes = context::context_scopes(Some(PROJECT), &id.device_id);
    let key = cache::cache_key(id.user_id, &spaces, &scopes, &id.device_id, "startup");
    let store = |rendered_at| {
        let (key, spaces, scopes) = (key.clone(), spaces.clone(), scopes.clone());
        let w = &w;
        async move {
            let mut tx = memvault_core::user_tx(&w.pool, &w.alice.identity).await.unwrap();
            let entry = cache::CacheEntry {
                key: &key,
                identity: &w.alice.identity,
                spaces: &spaces,
                scopes: &scopes,
                project_key: Some(PROJECT),
                device_id: &w.alice.identity.device_id,
                source: "startup",
                rendered_at,
            };
            cache::store(&mut tx, &entry, "body").await.unwrap();
            tx.commit().await.unwrap();
        }
    };
    let lookup = || async {
        let mut tx = memvault_core::user_tx(&w.pool, &w.alice.identity).await.unwrap();
        cache::lookup(&mut tx, &key).await.unwrap()
    };

    let started = {
        let mut c = w.pool.acquire().await.unwrap();
        cache::render_start(&mut c).await.unwrap()
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    // The write commits and the listener handles its NOTIFY while the render is still running: 0 rows.
    let deleted =
        cache::invalidate(&w.pool, Some(w.alice.space_id), Some(("project", PROJECT)), Duration::from_secs(2))
            .await
            .unwrap();
    assert_eq!(deleted, 0);
    store(started).await;
    assert_eq!(cache_rows(&w.pool).await, 1);
    assert_eq!(lookup().await, None, "stale block rejected");

    // A render that began after the invalidation is fine; so is an unrelated scope's invalidation.
    let later = {
        let mut c = w.pool.acquire().await.unwrap();
        cache::render_start(&mut c).await.unwrap()
    };
    store(later).await;
    assert_eq!(lookup().await.as_deref(), Some("body"));
    cache::invalidate(&w.pool, Some(w.alice.space_id), Some(("project", "other")), Duration::from_secs(2))
        .await
        .unwrap();
    store(later).await;
    assert_eq!(lookup().await.as_deref(), Some("body"));
}
