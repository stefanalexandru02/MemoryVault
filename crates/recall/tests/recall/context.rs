//! SessionStart block, context cache, invalidation listener, worker re-render.

use std::time::Duration;

use memvault_core::auth::admin::ensure_client;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::ids::short_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::common::{
    ClientKind, InjectionMode, ObservationType, Scope, ScopeKind, Sensitivity, SummaryGrain,
};
use memvault_core::schemas::recall::UserSettings;
use memvault_core::system_tx;
use memvault_core::test_support::seed_identity;
use memvault_recall::{cache, context, render, settings};
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
    let md = context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", None).await.unwrap();

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
    assert!(md.contains("\n_Legend: ⚖ decision · ● bugfix_"), "only the glyphs shown: {md}");
    // Index: newest first, glyphs, short ids; subagent and secret observations hidden.
    let b = pos(&format!("| {} |", short_id(&s.bugfix)));
    let d = pos(&format!("| {} |", short_id(&s.decision)));
    assert!(b < d, "newest first");
    assert!(md.contains("| ⚖ | Chose pgvector over Chroma for the server |"), "{md}");
    assert!(md.contains("| ● | Fixed off-by-one in window close |"), "{md}");
    assert!(!md.contains(&short_id(&s.subagent)) && !md.contains(&short_id(&s.secret)));
    assert!(!md.contains("hunter2"));
    // Recent sessions (session summaries' next steps) from the cached part, without the session that
    // since-you-were-here already lists.
    let recent = pos("### Recent sessions\n");
    assert!(md[recent..].contains("Old work") && !md[recent..].contains("Add rerank stage to recall"), "{md}");
    assert_eq!(md.matches("Add rerank stage to recall").count(), 1, "{md}");

    // Cached: one row carrying what a re-render needs; a second call is identical.
    let (n, client, project_key): (i64, Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT count(*) OVER (), client_id, project_key FROM context_cache")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!((n, client, project_key.as_deref()), (1, Some(w.alice.client_id), Some(PROJECT)));
    let again =
        context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", None).await.unwrap();
    assert_eq!(md, again);
    // Served from the cache, with since-you-were-here still computed live.
    sqlx::query("UPDATE context_cache SET body_md = '### Open work\n- CACHED'").execute(&w.pool).await.unwrap();
    let cached =
        context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", None).await.unwrap();
    assert!(cached.contains("### Since you were here") && cached.ends_with("### Open work\n- CACHED"), "{cached}");
    // A stale row (older than the max age) is rendered again.
    sqlx::query("UPDATE context_cache SET rendered_at = now() - interval '2 hours'").execute(&w.pool).await.unwrap();
    let fresh =
        context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", None).await.unwrap();
    assert_eq!(fresh, md);

    // Other sources are separate cache keys; resume returns nothing.
    context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "clear", None).await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM context_cache").fetch_one(&w.pool).await.unwrap();
    assert_eq!(rows, 2);
    assert_eq!(
        context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "resume", None).await.unwrap(),
        ""
    );
    // The retrieval log records every context call: three renders, then the two served from the cache.
    let rows: Vec<(String, Option<String>, i32, i32)> = sqlx::query_as(
        "SELECT mode, injection, tokens, jsonb_array_length(selected) FROM retrieval_log ORDER BY created_at, id",
    )
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert!(rows.iter().all(|r| r.0 == "context" && r.1.as_deref() == Some("full") && r.2 > 0), "{rows:?}");
    let selected: Vec<bool> = rows.iter().map(|r| r.3 > 0).collect();
    assert_eq!(selected, [true, false, false, true, true], "cache hits rank nothing: {rows:?}");
    let hits: Vec<i32> =
        sqlx::query_scalar("SELECT hits FROM retrieval_log ORDER BY created_at, id").fetch_all(&w.pool).await.unwrap();
    assert!(hits[0] > 0 && hits[1] == hits[0] && hits[2] == hits[0], "a hit repeats what the block selected: {hits:?}");
    let _ = s.desktop;
}

#[sqlx::test(migrations = "../../migrations")]
async fn context_without_project_or_data(pool: PgPool) {
    let w = world(pool).await;
    let md = context::render_context(&w.state, &w.alice.identity, None, None, "startup", None).await.unwrap();
    assert_eq!(md, "## Memory\n_No memories for this project yet._");
    // Device-scoped rows show up under the device given in the query.
    let mut o = Obs::new(w.alice.space_id, "Laptop battery tweak", "tlp settings");
    o.scope = Scope::new(ScopeKind::Device, "laptop-x1");
    let id = insert_obs(&w, o).await;
    let md =
        context::render_context(&w.state, &w.alice.identity, None, Some("laptop-x1"), "startup", None).await.unwrap();
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
    let md =
        context::render_context(&w.state, &w.alice.identity, None, Some("laptop-x1"), "startup", None).await.unwrap();
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
    let first =
        context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", None).await.unwrap();
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
    context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", None).await.unwrap();
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
    context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", None).await.unwrap();
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
    let variant = cache::Variant { mode: InjectionMode::Full, rows: 12, types: vec![] };
    let key = cache::cache_key(id.user_id, &spaces, &scopes, &id.device_id, "startup", &variant);
    let store = |rendered_at| {
        let (key, spaces, scopes, variant) = (key.clone(), spaces.clone(), scopes.clone(), variant.clone());
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
                variant: &variant,
                hits: 0,
                rendered_at,
            };
            cache::store(&mut tx, &entry, "body").await.unwrap();
            tx.commit().await.unwrap();
        }
    };
    let lookup = || async {
        let mut tx = memvault_core::user_tx(&w.pool, &w.alice.identity).await.unwrap();
        cache::lookup(&mut tx, &key).await.unwrap().map(|c| c.body)
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

#[sqlx::test(migrations = "../../migrations")]
async fn lean_block_is_profile_open_work_and_rubric(pool: PgPool) {
    let w = world(pool).await;
    let s = seed(&w).await;
    let lean = InjectionMode::Lean;
    let md =
        context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", Some(lean)).await.unwrap();
    assert!(
        md.starts_with("## Memory\n### Profile\n- prefers.vector_store: pgvector for personal projects [fact#"),
        "{md}"
    );
    assert!(md.contains("### Open work\n- todo/homelab: api deploy ☐ · migrations ✓\n"), "{md}");
    assert!(md.ends_with(render::LEAN_RUBRIC), "{md}");
    for gone in ["Since you were here", "Relevant procedures", "Recent sessions", render::RECENT_OBSERVATIONS_TITLE] {
        assert!(!md.contains(gone), "{gone} is not in the lean block:\n{md}");
    }
    assert!(!md.contains(&short_id(&s.decision)) && !md.contains("Deploy memory-api"), "{md}");
    let tokens = memvault_core::estimate_tokens(&md);
    assert!((100..=450).contains(&tokens), "a lean block is ~400 tokens, got {tokens}:\n{md}");

    // Full and lean blocks are cached apart; neither is served for the other.
    let full =
        context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", None).await.unwrap();
    assert!(full.contains("### Since you were here") && !full.contains("### Memory tools"), "{full}");
    let modes: Vec<String> =
        sqlx::query_scalar("SELECT mode FROM context_cache ORDER BY mode").fetch_all(&w.pool).await.unwrap();
    assert_eq!(modes, ["full", "lean"]);
    let again =
        context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", Some(lean)).await.unwrap();
    assert_eq!(again, md);

    // Logged like a full call, under its mode; the hit repeats the render's counts.
    let rows: Vec<(Option<String>, i32, i32)> = sqlx::query_as(
        "SELECT injection, tokens, hits FROM retrieval_log WHERE injection = 'lean' ORDER BY created_at, id",
    )
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0], rows[1]);
    assert_eq!((rows[0].1, rows[0].2), (tokens, 1), "the pinned fact is the one memory selected");

    // Nothing to remember still gets the rubric; a resume stays empty.
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let empty = context::render_context(&w.state, &bob.identity, None, None, "startup", Some(lean)).await.unwrap();
    assert_eq!(empty, format!("## Memory\n{}", render::LEAN_RUBRIC));
    let resume = context::render_context(&w.state, &w.alice.identity, None, None, "resume", Some(lean)).await.unwrap();
    assert_eq!(resume, "");
}

#[sqlx::test(migrations = "../../migrations")]
async fn injection_mode_precedence_and_per_mode_rerender(pool: PgPool) {
    let w = world(pool).await;
    seed(&w).await;
    let id = &w.alice.identity;
    let render = |mode| context::render_context(&w.state, id, Some(PROJECT), None, "startup", mode);

    // Request > user default > full.
    assert!(render(None).await.unwrap().contains("### Recent sessions"), "full without any setting");
    settings::put(&w.state, id, UserSettings { injection_mode: InjectionMode::Lean }).await.unwrap();
    assert!(render(None).await.unwrap().ends_with(render::LEAN_RUBRIC), "the user's default applies");
    let full = render(Some(InjectionMode::Full)).await.unwrap();
    assert!(full.contains("### Recent sessions"), "the request wins over the default");
    // Changing the default needs no invalidation: the mode is part of the cache key.
    settings::put(&w.state, id, UserSettings { injection_mode: InjectionMode::Full }).await.unwrap();
    assert_eq!(render(None).await.unwrap(), full);
    assert_eq!(settings::get(&w.state, id).await.unwrap().injection_mode, InjectionMode::Full);

    // An invalidated lean block is queued, and re-rendered, as a lean block.
    render(Some(InjectionMode::Lean)).await.unwrap();
    let space = w.alice.space_id;
    assert_eq!(cache::handle_payload(&w.state, &format!("{space}:project:{PROJECT}")).await.unwrap(), 2);
    let jobs: Vec<Value> =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE type = 'render_context' ORDER BY payload->>'mode'")
            .fetch_all(&w.pool)
            .await
            .unwrap();
    let job_modes: Vec<&str> = jobs.iter().map(|j| j["mode"].as_str().unwrap()).collect();
    assert_eq!(job_modes, ["full", "lean"]);
    assert!(cache::rerender(&w.state, &jobs[1]).await.unwrap());
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT mode, body_md FROM context_cache").fetch_all(&w.pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "lean");
    assert!(rows[0].1.ends_with(render::LEAN_RUBRIC));
    // A job queued before modes existed has none and renders the full block.
    let mut legacy = jobs[0].clone();
    legacy.as_object_mut().unwrap().remove("mode");
    assert!(cache::rerender(&w.state, &legacy).await.unwrap());
    let modes: Vec<String> =
        sqlx::query_scalar("SELECT mode FROM context_cache ORDER BY mode").fetch_all(&w.pool).await.unwrap();
    assert_eq!(modes, ["full", "lean"]);
}

const FAMILIES: [(&str, &str); 6] = [
    ("decision", "Chose pgvector over Chroma for the server"),
    ("bugfix", "Fixed off-by-one in window close"),
    ("feature", "Added idempotency table for webhook events"),
    ("refactor", "Split the invoice module into builder and renderer"),
    ("change", "Moved staging to a smaller Fly machine"),
    ("discovery", "Webhook retries arrive every fifteen minutes"),
];

/// Two observations of each family, the second a variant of the first, newest first.
async fn seed_families(w: &World) {
    for (i, (kind, title)) in FAMILIES.iter().enumerate() {
        for variant in 0..2 {
            let mut o = Obs::new(w.alice.space_id, &format!("{title} (take {})", variant + 1), "narrative");
            o.obs_type = kind;
            o.age_days = 0.1 + i as f64 * 0.1 + f64::from(variant) * 0.01;
            o.embed = false;
            insert_obs(w, o).await;
        }
    }
}

fn index_rows(md: &str) -> usize {
    md.lines().filter(|l| l.starts_with("| ") && !l.starts_with("| id ")).count()
}

#[sqlx::test(migrations = "../../migrations")]
async fn index_rows_and_types_shape_the_block_and_the_cache(pool: PgPool) {
    let w = world(pool).await;
    seed_families(&w).await;
    let id = &w.alice.identity;
    let state = &w.state;
    let render = |rows: Option<u32>, types: Vec<ObservationType>| async move {
        let options = context::ContextOptions { mode: Some(InjectionMode::Full), rows, types };
        context::render_context_with(state, id, Some(PROJECT), None, "startup", &options).await
    };

    // The second take of a family is a near-duplicate of the first, so six families make six rows however many are
    // asked; asking for none is the default size.
    for rows in [None, Some(30)] {
        assert_eq!(index_rows(&render(rows, vec![]).await.unwrap()), 6);
    }
    // Fewer rows keep the best, at most one of a type per third; the cache holds one block per size.
    assert_eq!(index_rows(&render(Some(2), vec![]).await.unwrap()), 2);
    assert_eq!(cache_rows(&w.pool).await, 3, "a block per size: the default, 30 and 2");
    let decisions = render(None, vec![ObservationType::Decision, ObservationType::Decision]).await.unwrap();
    assert_eq!(index_rows(&decisions), 1);
    assert!(decisions.contains("pgvector") && !decisions.contains("off-by-one"), "{decisions}");
    let both = render(None, vec![ObservationType::Bugfix, ObservationType::Decision]).await.unwrap();
    assert_eq!(index_rows(&both), 2);
    // The same request again is a cache hit, in any order of its types; each variant has its own block.
    let before = cache_rows(&w.pool).await;
    assert_eq!(render(None, vec![ObservationType::Decision, ObservationType::Bugfix]).await.unwrap(), both);
    assert_eq!(cache_rows(&w.pool).await, before);

    // An invalidated block is re-rendered as it was: the job carries its rows and types.
    let space = w.alice.space_id;
    cache::handle_payload(&w.state, &format!("{space}:project:{PROJECT}")).await.unwrap();
    let jobs: Vec<Value> =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE type = 'render_context'").fetch_all(&w.pool).await.unwrap();
    let job = jobs.iter().find(|j| j["obs_types"] == json!(["bugfix", "decision"])).expect("a job with the types");
    assert_eq!(job["obs_rows"], 12);
    assert!(cache::rerender(&w.state, job).await.unwrap());
    let stored: (i32, Vec<String>) =
        sqlx::query_as("SELECT obs_rows, obs_types FROM context_cache").fetch_one(&w.pool).await.unwrap();
    assert_eq!(stored, (12, vec!["bugfix".to_string(), "decision".to_string()]));
    // A job queued before the index was configurable renders the default one.
    let mut legacy = job.clone();
    legacy.as_object_mut().unwrap().retain(|k, _| !k.starts_with("obs_"));
    assert!(cache::rerender(&w.state, &legacy).await.unwrap());

    // The lean block has no index, so rows and types do not split its cache.
    let lean = |rows: Option<u32>| async move {
        let options =
            context::ContextOptions { mode: Some(InjectionMode::Lean), rows, types: vec![ObservationType::Bugfix] };
        context::render_context_with(state, id, Some(PROJECT), None, "startup", &options).await
    };
    sqlx::query("DELETE FROM context_cache").execute(&w.pool).await.unwrap();
    assert_eq!(lean(Some(3)).await.unwrap(), lean(None).await.unwrap());
    assert_eq!(cache_rows(&w.pool).await, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_index_mixes_types_instead_of_filling_up_with_one(pool: PgPool) {
    let w = world(pool).await;
    // Ten distinct bug fixes, newer than three other notes: by recency alone the index would be all bug fixes.
    let words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet"];
    for (i, word) in words.iter().enumerate() {
        let mut o =
            Obs::new(w.alice.space_id, &format!("Fixed {word} crash in the {} parser {i}", words[(i + 3) % 10]), "n");
        o.obs_type = "bugfix";
        o.age_days = 0.1 + i as f64 * 0.01;
        o.embed = false;
        insert_obs(&w, o).await;
    }
    for (kind, title) in [
        ("decision", "Chose Postgres for storage"),
        ("feature", "Added a CSV export"),
        ("discovery", "Cache misses spike at noon"),
    ] {
        let mut o = Obs::new(w.alice.space_id, title, "n");
        o.obs_type = kind;
        o.age_days = 2.0;
        o.embed = false;
        insert_obs(&w, o).await;
    }
    let options = context::ContextOptions { mode: Some(InjectionMode::Full), rows: Some(8), types: vec![] };
    let md = context::render_context_with(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", &options)
        .await
        .unwrap();
    assert_eq!(index_rows(&md), 8);
    // Three bug fixes (a third of the rows), the other types, then the bug fixes the limit held back.
    for title in ["Chose Postgres", "Added a CSV export", "Cache misses"] {
        assert!(md.contains(title), "{title} missing:\n{md}");
    }
    assert_eq!(md.matches("| ● |").count(), 5, "{md}");
}
