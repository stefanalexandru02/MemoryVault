//! Recall engine: ranking, budgets, sensitivity, scopes, RLS, determinism, fallbacks.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use memvault_core::auth::scopes::TokenKind;
use memvault_core::schemas::common::{MembershipRole, MemoryTable, Scope, ScopeKind, Sensitivity, SummaryGrain};
use memvault_core::schemas::memories::{ByFileQuery, MemoryItem, SearchQuery, TimelineQuery};
use memvault_core::schemas::recall::{Budgets, FeedbackRequest, RecallRequest, TaskState};
use memvault_core::tei::{Embedder, FakeTei, TeiError, TeiHealth};
use memvault_core::test_support::{seed_identity, seed_shared_space};
use memvault_recall::engine;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::*;

fn req(query: &str) -> RecallRequest {
    RecallRequest {
        query: Some(query.into()),
        scopes: Some(vec![Scope::global(), project()]),
        ..RecallRequest::default()
    }
}

fn obs_ids(resp: &memvault_core::schemas::recall::RecallResponse) -> Vec<Uuid> {
    resp.slots.observations.iter().map(|o| o.item.id).collect()
}

async fn seed_corpus(w: &World) -> (Uuid, Uuid, Uuid) {
    let space = w.alice.space_id;
    let hit = insert_obs(
        w,
        Obs::new(
            space,
            "Chose pgvector HNSW index for recall",
            "pgvector hnsw index with cosine distance beats chroma",
        ),
    )
    .await;
    let other = insert_obs(w, Obs::new(space, "Fixed flaky CI cache", "the github actions cache key was wrong")).await;
    let weak = insert_obs(w, Obs::new(space, "Tuned postgres index settings", "autovacuum and work_mem changes")).await;
    (hit, other, weak)
}

#[sqlx::test(migrations = "../../migrations")]
async fn ranks_the_relevant_observation_first(pool: PgPool) {
    let w = world(pool).await;
    let (hit, other, _) = seed_corpus(&w).await;
    let resp =
        engine::recall(&w.state, &w.alice.identity, req("which pgvector hnsw index did we choose")).await.unwrap();
    let ids = obs_ids(&resp);
    assert_eq!(ids.first(), Some(&hit), "{:#?}", resp.slots.observations);
    assert!(!ids.contains(&other) || ids.iter().position(|i| *i == other) > Some(0));
    assert!(resp.reranked);
    let md = resp.rendered_markdown.unwrap();
    assert!(md.starts_with("## Memory\n"), "{md}");
    assert!(md.contains("Chose pgvector HNSW index for recall"), "{md}");
    let tag = format!("obs#{}", memvault_core::ids::short_id(&hit));
    assert!(md.contains(&tag), "{md}");
    // Top observations are expanded (narrative present), the rest go into the index.
    assert!(resp.slots.observations[0].expanded);
    assert!(resp.slots.observations[0].narrative.is_some());
    // retrieval_log row with candidates and selected ids.
    let (selected, cands, reranked): (Value, Value, bool) =
        sqlx::query_as("SELECT selected, candidates, reranked FROM retrieval_log WHERE id = $1")
            .bind(resp.retrieval_id)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert!(reranked);
    assert!(selected.as_array().unwrap().iter().any(|s| s["id"] == json!(hit) && s["slot"] == "observations"));
    assert!(cands.as_array().unwrap().iter().any(|c| c["id"] == json!(hit) && c.get("rerank").is_some()));
}

#[sqlx::test(migrations = "../../migrations")]
async fn keyword_only_when_tei_is_down(pool: PgPool) {
    let w = world(pool).await;
    let (hit, ..) = seed_corpus(&w).await;
    w.tei.set_down(true);
    let resp = engine::recall(&w.state, &w.alice.identity, req("pgvector hnsw")).await.unwrap();
    assert!(!resp.reranked);
    assert_eq!(obs_ids(&resp).first(), Some(&hit));
    let cands: Value = sqlx::query_scalar("SELECT candidates FROM retrieval_log WHERE id = $1")
        .bind(resp.retrieval_id)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    // Keyword leg only: no cosine, no rerank.
    assert!(cands.as_array().unwrap().iter().all(|c| c.get("cos").is_none() && c.get("rerank").is_none()));
    assert!(cands.as_array().unwrap().iter().any(|c| c.get("bm25").is_some()));
}

#[sqlx::test(migrations = "../../migrations")]
async fn vector_leg_and_its_recency_window(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    // The "alias" part is embedded but not in search_tsv, so "quokka marsupial" reaches these rows
    // through the vector leg only.
    let mut old = Obs::new(space, "Habitat notes", "rottnest island");
    old.age_days = 200.0;
    let old_scope = old.scope.clone();
    let old = insert_obs(&w, old).await;
    embed(&w, space, "observations", old, "alias", &old_scope, "quokka marsupial").await;
    let fresh = insert_obs(&w, Obs::new(space, "Diet notes", "leaves and grass")).await;
    embed(&w, space, "observations", fresh, "alias", &project(), "quokka marsupial").await;
    let r = RecallRequest { rerank: false, ..req("quokka marsupial") };
    let ids = obs_ids(&engine::recall(&w.state, &w.alice.identity, r).await.unwrap());
    assert_eq!(ids.first(), Some(&fresh), "best part wins");
    assert!(!ids.contains(&old), "the 90-day window applies to the vector leg");
    // The keyword leg has no window: an exact keyword hit older than 90 days still surfaces (recency
    // decay still ranks it below fresh rows of equal fusion rank, so the reranker decides).
    let r = RecallRequest { rerank: false, ..req("rottnest island") };
    let ids = obs_ids(&engine::recall(&w.state, &w.alice.identity, r).await.unwrap());
    assert!(ids.contains(&old));
    let ids = obs_ids(&engine::recall(&w.state, &w.alice.identity, req("rottnest island")).await.unwrap());
    assert_eq!(ids.first(), Some(&old));
}

/// Embeds like FakeTei but takes 1 s to rerank.
struct SlowRerank(FakeTei);

#[async_trait]
impl Embedder for SlowRerank {
    fn model(&self) -> &str {
        self.0.model()
    }
    fn dims(&self) -> usize {
        self.0.dims()
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, TeiError> {
        self.0.embed(texts).await
    }
    async fn rerank(&self, query: &str, texts: &[String]) -> Result<Vec<f32>, TeiError> {
        tokio::time::sleep(Duration::from_secs(1)).await;
        self.0.rerank(query, texts).await
    }
    async fn health(&self) -> TeiHealth {
        self.0.health().await
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn rerank_timeout_falls_back_to_rrf(pool: PgPool) {
    let mut w = world(pool).await;
    let (hit, ..) = seed_corpus(&w).await;
    w.state.tei = Arc::new(SlowRerank(FakeTei::default()));
    let started = std::time::Instant::now();
    let resp = engine::recall(&w.state, &w.alice.identity, req("pgvector hnsw index")).await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(900), "the 250 ms rerank timeout applies");
    assert!(!resp.reranked);
    assert_eq!(obs_ids(&resp).first(), Some(&hit));
    // rerank=false never calls the reranker.
    let w2 = world(w.pool.clone()).await;
    seed_corpus(&w2).await;
    let r = RecallRequest { rerank: false, ..req("pgvector") };
    engine::recall(&w2.state, &w2.alice.identity, r).await.unwrap();
    assert!(w2.tei.calls.lock().unwrap().iter().all(|(m, _)| *m != "rerank"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn budgets_bound_every_slot(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    for i in 0..15 {
        insert_obs(
            &w,
            Obs::new(space, &format!("Kubernetes deploy note {i}"), &"kubernetes deploy rollout ".repeat(3)),
        )
        .await;
        insert_fact(&w, Fact::new(space, &format!("prefers.tool_{i}"), "kubernetes deploy with helm")).await;
    }
    let budgets = Budgets { profile: 0, facts: 40, procedures: 0, observations: 120, summaries: 0, work_state: 0 };
    let r = RecallRequest { budgets, expand_top_observations: 1, ..req("kubernetes deploy") };
    let resp = engine::recall(&w.state, &w.alice.identity, r).await.unwrap();
    let by = &resp.tokens.by_slot;
    assert!(by["observations"] <= 120 && by["observations"] > 0, "{by:?}");
    assert!(by["facts"] <= 40 && by["facts"] > 0, "{by:?}");
    assert_eq!(by["profile"], 0);
    assert!(resp.slots.profile.is_none());
    assert!(resp.slots.procedures.is_empty() && resp.slots.summaries.is_empty());
    assert_eq!(resp.tokens.total, by.values().sum::<i32>());
    assert!(resp.slots.observations.len() < 15);
    assert!(resp.slots.facts.len() < 15);
    // Only the first observation is expanded.
    assert_eq!(resp.slots.observations.iter().filter(|o| o.expanded).count(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn sensitivity_exclusion(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let mut secret = Obs::new(space, "Vault root token location", "vault root token lives in the safe");
    secret.sensitivity = Sensitivity::Secret;
    let secret = insert_obs(&w, secret).await;
    let mut sensitive = Obs::new(space, "Vault unseal procedure", "vault unseal with three keys");
    sensitive.sensitivity = Sensitivity::Sensitive;
    let sensitive = insert_obs(&w, sensitive).await;
    let mut secret_fact = Fact::new(space, "vault.root_token", "vault token in the safe");
    secret_fact.sensitivity = Sensitivity::Secret;
    let secret_fact = insert_fact(&w, secret_fact).await;

    let resp = engine::recall(&w.state, &w.alice.identity, req("vault token unseal")).await.unwrap();
    let ids = obs_ids(&resp);
    assert!(!ids.contains(&secret) && ids.contains(&sensitive));
    assert!(resp.slots.facts.iter().all(|f| f.item.id != secret_fact));
    assert!(!resp.rendered_markdown.unwrap().contains("root token"));

    let r = RecallRequest { exclude_sensitivity: vec![Sensitivity::Secret, Sensitivity::Sensitive], ..req("vault") };
    assert!(obs_ids(&engine::recall(&w.state, &w.alice.identity, r).await.unwrap()).is_empty());

    // Search never lists secrets either.
    let q = SearchQuery { q: Some("vault".into()), ..SearchQuery::default() };
    let rows = engine::search(&w.state, &w.alice.identity, SearchQuery { limit: 20, ..q }).await.unwrap().rows;
    assert!(rows.iter().all(|r| r.id != secret) && rows.iter().any(|r| r.id == sensitive));
}

#[sqlx::test(migrations = "../../migrations")]
async fn scope_filtering(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let mut a = Obs::new(space, "Grafana dashboard for memory api", "grafana dashboard panels");
    a.scope = Scope::new(ScopeKind::Project, "github.com/alex/a");
    let a = insert_obs(&w, a).await;
    let mut b = Obs::new(space, "Grafana dashboard for billing", "grafana dashboard panels");
    b.scope = Scope::new(ScopeKind::Project, "github.com/alex/b");
    let b = insert_obs(&w, b).await;
    let r = RecallRequest {
        scopes: Some(vec![Scope::new(ScopeKind::Project, "github.com/alex/a")]),
        ..req("grafana dashboard")
    };
    let ids = obs_ids(&engine::recall(&w.state, &w.alice.identity, r).await.unwrap());
    assert_eq!(ids, vec![a]);
    assert!(!ids.contains(&b));
    // A global scope with a key is rejected.
    let r = RecallRequest { scopes: Some(vec![Scope::new(ScopeKind::Global, "x")]), ..req("grafana") };
    assert_eq!(engine::recall(&w.state, &w.alice.identity, r).await.unwrap_err().status, 422);
}

#[sqlx::test(migrations = "../../migrations")]
async fn rls_isolates_users_and_shares_spaces(pool: PgPool) {
    let w = world(pool).await;
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let private =
        insert_obs(&w, Obs::new(w.alice.space_id, "Alice private note on terraform", "terraform state")).await;
    let fact = insert_fact(&w, Fact::new(w.alice.space_id, "uses.terraform", "terraform cloud")).await;
    let shared = seed_shared_space(&w.pool, "team", w.alice.user_id, &[(bob.user_id, MembershipRole::Reader)]).await;
    let team = insert_obs(&w, Obs::new(shared, "Team terraform module layout", "terraform modules per env")).await;

    let resp = engine::recall(&w.state, &bob.identity, req("terraform")).await.unwrap();
    let ids = obs_ids(&resp);
    assert!(ids.contains(&team), "shared space is readable");
    assert!(!ids.contains(&private));
    assert!(resp.slots.facts.iter().all(|f| f.item.id != fact));
    // Explicitly asking for Alice's space is refused.
    let r = RecallRequest { space_ids: Some(vec![w.alice.space_id]), ..req("terraform") };
    let err = engine::recall(&w.state, &bob.identity, r).await.unwrap_err();
    assert_eq!((err.status.as_u16(), err.code.as_str()), (403, "space_forbidden"));
    // Search, get and explain see nothing of Alice's.
    let q = SearchQuery { q: Some("terraform".into()), limit: 20, ..SearchQuery::default() };
    let rows = engine::search(&w.state, &bob.identity, q).await.unwrap().rows;
    assert!(rows.iter().all(|r| r.id != private) && rows.iter().any(|r| r.id == team));
    let got = engine::get_memories(&w.state, &bob.identity, &[private, fact, team]).await.unwrap();
    assert_eq!(got.items.iter().map(MemoryItem::id).collect::<Vec<_>>(), vec![team]);
    assert_eq!(got.missing, vec![private, fact]);
    assert_eq!(engine::explain(&w.state, &bob.identity, private).await.unwrap_err().status, 404);
    // Alice sees both.
    let ids = obs_ids(&engine::recall(&w.state, &w.alice.identity, req("terraform")).await.unwrap());
    assert!(ids.contains(&private) && ids.contains(&team));
}

#[sqlx::test(migrations = "../../migrations")]
async fn recall_is_deterministic(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    for i in 0..12 {
        // Equal scores on purpose: ties break by id.
        insert_obs(&w, Obs::new(space, &format!("Redis eviction policy {i}"), "redis eviction allkeys lru")).await;
    }
    insert_fact(&w, Fact::new(space, "prefers.cache", "redis")).await;
    insert_procedure(&w, space, "Flush redis safely", "ops", "use redis-cli --scan then unlink").await;
    let a = engine::recall(&w.state, &w.alice.identity, req("redis eviction")).await.unwrap();
    let b = engine::recall(&w.state, &w.alice.identity, req("redis eviction")).await.unwrap();
    // Scores carry the wall-clock recency, so compare what was selected and rendered.
    let ids = |r: &memvault_core::schemas::recall::RecallResponse| {
        let s = &r.slots;
        (
            s.facts.iter().map(|i| i.item.id).collect::<Vec<_>>(),
            s.procedures.iter().map(|i| i.item.id).collect::<Vec<_>>(),
            s.observations.iter().map(|i| (i.item.id, i.expanded)).collect::<Vec<_>>(),
        )
    };
    assert_eq!(ids(&a), ids(&b));
    assert_eq!(a.slots.observations.len(), 12);
    assert_eq!(a.rendered_markdown, b.rendered_markdown);
    let selected: Vec<Value> =
        sqlx::query_scalar("SELECT selected FROM retrieval_log ORDER BY id").fetch_all(&w.pool).await.unwrap();
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0], selected[1]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn facts_profile_and_procedures(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let mut pinned = Fact::new(space, "timezone", "Europe/Bucharest");
    pinned.pinned = true;
    let pinned = insert_fact(&w, pinned).await;
    let editor = insert_fact(&w, Fact::new(space, "prefers.editor", "helix for rust code")).await;
    let deploy =
        insert_procedure(&w, space, "Deploy memory-api", "deploy", "steps: run migrations job, then roll").await;
    let rust = insert_procedure(&w, space, "Rust release build", "build", "steps: cargo build --release").await;

    // No profiles row yet: the fallback profile lists the top facts (pinned first).
    let r = RecallRequest {
        task_state: Some(TaskState { intent: Some("deploy".into()), entities: vec!["memory-api".into()] }),
        rerank: false,
        ..req("release steps")
    };
    let resp = engine::recall(&w.state, &w.alice.identity, r).await.unwrap();
    let profile = resp.slots.profile.clone().expect("fallback profile");
    assert!(profile.body_md.contains("timezone: Europe/Bucharest"), "{}", profile.body_md);
    assert!(profile.body_md.contains(&format!("[fact#{}", memvault_core::ids::short_id(&pinned))));
    // Facts shown in the profile are not repeated in the facts slot.
    assert!(resp.slots.facts.iter().all(|f| f.item.id != editor && f.item.id != pinned));
    // Intent match (+0.3) puts the deploy procedure first although the query fits the build one better.
    let procs: Vec<Uuid> = resp.slots.procedures.iter().map(|p| p.item.id).collect();
    assert_eq!(procs, vec![deploy, rust]);
    assert!(resp.rendered_markdown.unwrap().contains("**Deploy memory-api** (v1)"));

    // With a rendered profile, the profile slot uses it and pinned facts still show.
    sqlx::query(
        "INSERT INTO profiles (space_id, scope_kind, scope_key, body_md, token_estimate, facts_version)
         VALUES ($1, 'global', '', '- Prefers helix [fact#abcdef · 0.80 · 2026-10-01]', 10, 1)",
    )
    .bind(space)
    .execute(&w.pool)
    .await
    .unwrap();
    let resp = engine::recall(&w.state, &w.alice.identity, req("editor")).await.unwrap();
    assert_eq!(resp.slots.profile.unwrap().body_md, "- Prefers helix [fact#abcdef · 0.80 · 2026-10-01]");
    assert!(resp.slots.facts.iter().any(|f| f.item.id == pinned && f.pinned));
    let md = resp.rendered_markdown.unwrap();
    assert!(md.starts_with("## Memory\n### Profile\n- Prefers helix"), "{md}");
    assert!(md.contains("- timezone: Europe/Bucharest [fact#"), "{md}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn summaries_and_work_state(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let s =
        insert_summary(&w, space, &project(), None, SummaryGrain::Session, "add rerank stage", "wire tei", 1.0).await;
    let p = insert_summary(&w, space, &project(), None, SummaryGrain::Prompt, "add rerank prompt", "x", 1.0).await;
    insert_work_state(&w.pool, space, &project(), "todo", json!({"migrations": "done", "deploy": "todo"})).await;
    insert_work_state(
        &w.pool,
        space,
        &project(),
        "todo",
        json!({"deploy": "done", "evals": false, "migrations": null}),
    )
    .await;
    let resp = engine::recall(&w.state, &w.alice.identity, req("rerank stage")).await.unwrap();
    let sums: Vec<Uuid> = resp.slots.summaries.iter().map(|s| s.item.id).collect();
    assert!(sums.contains(&s) && !sums.contains(&p), "prompt summaries are not recalled");
    assert_eq!(resp.slots.work_state.len(), 1);
    let fields = &resp.slots.work_state[0].fields;
    assert_eq!(fields.get("deploy"), Some(&json!("done")));
    assert!(!fields.contains_key("migrations"));
    let md = resp.rendered_markdown.unwrap();
    assert!(md.contains("- todo/homelab: deploy ✓ · evals ☐"), "{md}");
    assert!(md.contains("→ next: wire tei"), "{md}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn search_timeline_by_file(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let mut ids = Vec::new();
    for i in 0..5 {
        let mut o = Obs::new(space, &format!("Step {i} of the nginx migration"), "nginx config");
        o.age_days = 5.0 - f64::from(i);
        o.obs_type = if i == 2 { "decision" } else { "change" };
        if i == 3 {
            o.files = vec!["deploy/nginx.conf".into()];
        }
        ids.push(insert_obs(&w, o).await);
    }
    let q = SearchQuery { q: Some("nginx migration".into()), limit: 3, ..SearchQuery::default() };
    let page = engine::search(&w.state, &w.alice.identity, q.clone()).await.unwrap();
    assert_eq!(page.rows.len(), 3);
    assert!(page.rows.iter().all(|r| r.score.is_some()));
    let next = engine::search(&w.state, &w.alice.identity, SearchQuery { offset: 3, ..q }).await.unwrap();
    assert_eq!(next.rows.len(), 2);
    assert!(next.rows.iter().all(|r| !page.rows.iter().any(|p| p.id == r.id)));
    // Filters without q: newest first.
    let q = SearchQuery { types: Some(vec!["decision".into()]), limit: 20, ..SearchQuery::default() };
    let rows = engine::search(&w.state, &w.alice.identity, q).await.unwrap().rows;
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ids[2]]);
    let q = SearchQuery { files: Some(vec!["deploy/nginx.conf".into()]), limit: 20, ..SearchQuery::default() };
    assert_eq!(engine::search(&w.state, &w.alice.identity, q).await.unwrap().rows[0].id, ids[3]);
    assert_eq!(engine::search(&w.state, &w.alice.identity, SearchQuery::default()).await.unwrap_err().status, 422);
    // The candidate window is bounded: no huge or overflowing offsets.
    for offset in [1000, 50_000_000, i64::MAX] {
        let q = SearchQuery { q: Some("pgvector".into()), offset, limit: 20, ..SearchQuery::default() };
        assert_eq!(engine::search(&w.state, &w.alice.identity, q).await.unwrap_err().status, 422, "{offset}");
    }
    let q = SearchQuery { q: Some("pgvector".into()), offset: 980, limit: 20, ..SearchQuery::default() };
    engine::search(&w.state, &w.alice.identity, q).await.unwrap();

    let t = engine::timeline(&w.state, &w.alice.identity, TimelineQuery { anchor_id: ids[2], before: 1, after: 5 })
        .await
        .unwrap();
    assert_eq!(t.rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ids[1], ids[2], ids[3], ids[4]]);
    assert!(t.rows[1].is_anchor && !t.rows[0].is_anchor);
    let missing = TimelineQuery { anchor_id: Uuid::now_v7(), before: 1, after: 1 };
    assert_eq!(engine::timeline(&w.state, &w.alice.identity, missing).await.unwrap_err().status, 404);

    let f = ByFileQuery { path: "/home/alex/homelab/deploy/nginx.conf".into(), project_key: None, limit: 15 };
    let rows = engine::by_file(&w.state, &w.alice.identity, f).await.unwrap().rows;
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ids[3]]);
    let f = ByFileQuery { path: "nginx.conf".into(), project_key: Some("github.com/other".into()), limit: 15 };
    assert!(engine::by_file(&w.state, &w.alice.identity, f).await.unwrap().rows.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn get_writes_access_log_and_explain_has_provenance(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let session = insert_session(&w.pool, space, w.alice.client_id, PROJECT, 30.0).await;
    let e1 = insert_event(&w.pool, space, session, 3).await;
    let e2 = insert_event(&w.pool, space, session, 4).await;
    let mut o = Obs::new(space, "User prefers helix", "said so twice");
    o.session = Some(session);
    o.source_event_ids = vec![e2, e1];
    let obs = insert_obs(&w, o).await;
    let mut f = Fact::new(space, "prefers.editor", "helix");
    f.evidence = vec![obs];
    f.client = Some(w.alice.client_id);
    let fact = insert_fact(&w, f).await;
    sqlx::query(
        "INSERT INTO memory_audit (id, space_id, actor, action, target_table, target_id, reason)
         VALUES ($1, $2, 'planner', 'fact.create', 'facts', $3, 'auto-activated')",
    )
    .bind(Uuid::now_v7())
    .bind(space)
    .bind(fact)
    .execute(&w.pool)
    .await
    .unwrap();

    let got = engine::get_memories(&w.state, &w.alice.identity, &[fact, obs, fact]).await.unwrap();
    assert_eq!(got.items.iter().map(MemoryItem::id).collect::<Vec<_>>(), vec![fact, obs]);
    let logged: Vec<Uuid> = sqlx::query_scalar("SELECT unnest(target_ids) FROM access_log WHERE action = 'get'")
        .fetch_all(&w.pool)
        .await
        .unwrap();
    assert_eq!(logged, vec![fact, obs]);
    let md = memvault_recall::render::memory_items_markdown(&got.items);
    assert!(md.contains("#### ≡ alex prefers.editor [fact#"), "{md}");

    let e = engine::explain(&w.state, &w.alice.identity, fact).await.unwrap();
    assert_eq!(e.evidence.len(), 1);
    let p = &e.evidence[0].provenance;
    assert_eq!(p.session.as_ref().unwrap().device_id.as_deref(), Some(w.alice.device_id.as_str()));
    assert_eq!(p.event_seqs, vec![3, 4]);
    assert_eq!(p.extracted_by["prompt"], "observer.code@v1");
    assert_eq!(e.audit.len(), 1);
    assert_eq!(e.audit[0].action, "fact.create");
    let md = memvault_recall::render::explain_markdown(&e);
    assert!(md.contains("- events seq 3, 4") && md.contains("fact.create by planner: auto-activated"), "{md}");
    let e = engine::explain(&w.state, &w.alice.identity, obs).await.unwrap();
    assert!(e.evidence.is_empty());
    assert_eq!(e.provenance.unwrap().session.unwrap().project_key.as_deref(), Some(PROJECT));
}

#[sqlx::test(migrations = "../../migrations")]
async fn feedback_bumps_used_facts(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let fact = insert_fact(&w, Fact::new(space, "prefers.shell", "fish shell")).await;
    let r = RecallRequest { budgets: Budgets { profile: 0, ..Budgets::default() }, ..req("fish shell") };
    let resp = engine::recall(&w.state, &w.alice.identity, r).await.unwrap();
    assert!(resp.slots.facts.iter().any(|f| f.item.id == fact));
    // Not part of the retrieval.
    let unrelated = insert_fact(&w, Fact::new(space, "prefers.font", "iosevka")).await;
    let fb = FeedbackRequest { used_ids: vec![fact, unrelated], helpful: Some(true), notes: None };
    let out = engine::feedback(&w.state, &w.alice.identity, resp.retrieval_id, fb.clone()).await.unwrap();
    assert!(out.recorded);
    let counts: Vec<(Uuid, i32)> =
        sqlx::query_as("SELECT id, access_count FROM facts ORDER BY id").fetch_all(&w.pool).await.unwrap();
    assert!(counts.contains(&(fact, 1)));
    assert!(counts.contains(&(unrelated, 0)), "only ids the retrieval selected are bumped");
    let stored: Value = sqlx::query_scalar("SELECT feedback FROM retrieval_log WHERE id = $1")
        .bind(resp.retrieval_id)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(stored["helpful"], true);
    // Another client may not report on this retrieval.
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    assert_eq!(engine::feedback(&w.state, &bob.identity, resp.retrieval_id, fb).await.unwrap_err().status, 404);
}

/// Reranker giving every text the same low score.
struct LowRerank(FakeTei);

#[async_trait]
impl Embedder for LowRerank {
    fn model(&self) -> &str {
        self.0.model()
    }
    fn dims(&self) -> usize {
        self.0.dims()
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, TeiError> {
        self.0.embed(texts).await
    }
    async fn rerank(&self, _query: &str, texts: &[String]) -> Result<Vec<f32>, TeiError> {
        Ok(vec![0.001; texts.len()])
    }
    async fn health(&self) -> TeiHealth {
        self.0.health().await
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn rows_beyond_the_reranked_top_never_outrank_reranked_ones(pool: PgPool) {
    let mut w = world(pool).await;
    w.state.tei = Arc::new(LowRerank(FakeTei::default()));
    let space = w.alice.space_id;
    for i in 0..40 {
        insert_obs(&w, Obs::new(space, &format!("Quokka note {i}"), "quokka habitat survey")).await;
    }
    for i in 0..10 {
        insert_summary(&w, space, &project(), None, SummaryGrain::Session, &format!("quokka survey {i}"), "next", 1.0)
            .await;
    }
    let budgets = Budgets { profile: 0, facts: 0, procedures: 0, observations: 20000, summaries: 20000, work_state: 0 };
    let r = RecallRequest { budgets, ..req("quokka") };
    let resp = engine::recall(&w.state, &w.alice.identity, r).await.unwrap();
    assert!(resp.reranked);
    let (selected, cands): (Value, Value) =
        sqlx::query_as("SELECT selected, candidates FROM retrieval_log WHERE id = $1")
            .bind(resp.retrieval_id)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    let reranked: Vec<&Value> =
        cands.as_array().unwrap().iter().filter(|c| c.get("rerank").is_some()).map(|c| &c["id"]).collect();
    assert_eq!(reranked.len(), 40, "only the fused top 40 are reranked");
    let selected = selected.as_array().unwrap();
    assert!(!selected.is_empty());
    for s in selected {
        assert!(reranked.contains(&&s["id"]), "non-reranked row selected: {s}");
    }
}

#[test]
fn ef_search_covers_the_vector_leg_limit() {
    use memvault_recall::candidates::{VECTOR_LIMIT, VECTOR_PART_FANOUT};
    assert!(engine::ef_search_for(VECTOR_LIMIT) as i64 >= VECTOR_LIMIT * VECTOR_PART_FANOUT);
    assert_eq!(engine::ef_search_for(1), 80);
    assert_eq!(engine::ef_search_for(10_000), 1000);
}

#[sqlx::test(migrations = "../../migrations")]
async fn recall_and_context_write_access_log_for_returned_content(pool: PgPool) {
    let w = world(pool).await;
    let (hit, ..) = seed_corpus(&w).await;
    let mut pinned = Fact::new(w.alice.space_id, "prefers.editor", "helix");
    pinned.pinned = true;
    let fact = insert_fact(&w, pinned).await;
    let resp =
        engine::recall(&w.state, &w.alice.identity, req("which pgvector hnsw index did we choose")).await.unwrap();
    assert!(resp.slots.observations[0].expanded && resp.slots.observations[0].item.id == hit);
    let logged: Vec<Uuid> = sqlx::query_scalar("SELECT unnest(target_ids) FROM access_log WHERE action = 'recall'")
        .fetch_all(&w.pool)
        .await
        .unwrap();
    assert!(logged.contains(&hit), "the expanded observation is a content read: {logged:?}");
    assert!(logged.contains(&fact), "so is a returned fact: {logged:?}");
    let index_only: Vec<Uuid> = resp.slots.observations.iter().filter(|o| !o.expanded).map(|o| o.item.id).collect();
    assert!(index_only.iter().all(|id| !logged.contains(id)), "index lines are not content reads");

    memvault_recall::context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup")
        .await
        .unwrap();
    let ctx: Vec<Uuid> = sqlx::query_scalar("SELECT unnest(target_ids) FROM access_log WHERE action = 'context'")
        .fetch_all(&w.pool)
        .await
        .unwrap();
    assert!(ctx.contains(&fact), "{ctx:?}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn pinned_facts_compete_for_the_budget_by_importance_not_by_id(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let mut ids = Vec::new();
    // oldest id first, importance rising: the old ids must not win just by being old
    for (i, importance) in [0.1_f32, 0.5, 0.9].into_iter().enumerate() {
        let mut f = Fact::new(space, &format!("pinned.rule_{i}"), "keep this rule in mind always");
        f.pinned = true;
        let id = insert_fact(&w, f).await;
        sqlx::query("UPDATE facts SET importance = $2 WHERE id = $1")
            .bind(id)
            .bind(importance)
            .execute(&w.pool)
            .await
            .unwrap();
        ids.push(id);
    }
    let all = Budgets { profile: 0, facts: 1000, procedures: 0, observations: 0, summaries: 0, work_state: 0 };
    let full = engine::recall(&w.state, &w.alice.identity, RecallRequest { budgets: all, ..req("zzzz nothing") })
        .await
        .unwrap();
    assert_eq!(full.slots.facts.len(), 3);
    let one_and_a_half = full.tokens.by_slot["facts"] / 3 * 3 / 2;
    let tight = Budgets { facts: one_and_a_half as u32, ..all };
    let resp = engine::recall(&w.state, &w.alice.identity, RecallRequest { budgets: tight, ..req("zzzz nothing") })
        .await
        .unwrap();
    let got: Vec<Uuid> = resp.slots.facts.iter().map(|f| f.item.id).collect();
    assert_eq!(got, [ids[2]], "the most important pinned fact is the one that fits");
}

fn at(rfc3339: &str) -> Option<DateTime<Utc>> {
    Some(rfc3339.parse().unwrap())
}

/// Sorted ids a search finds in `tables` as of `as_of`.
async fn found_as_of(w: &World, q: &str, tables: &[MemoryTable], as_of: Option<DateTime<Utc>>) -> Vec<Uuid> {
    let q =
        SearchQuery { q: Some(q.into()), tables: Some(tables.to_vec()), as_of, limit: 50, ..SearchQuery::default() };
    let mut ids: Vec<Uuid> =
        engine::search(&w.state, &w.alice.identity, q).await.unwrap().rows.into_iter().map(|r| r.id).collect();
    ids.sort();
    ids
}

#[sqlx::test(migrations = "../../migrations")]
async fn as_of_search_sees_facts_as_they_were(pool: PgPool) {
    let w = world(pool).await;
    let h = insert_editor_history(&w).await;
    let search = |as_of| found_as_of(&w, "editor", &[MemoryTable::Facts], as_of);
    // Today only the current fact counts.
    assert_eq!(search(None).await, vec![h.helix]);
    // Before the switch the superseded fact is back in force (its null valid_from falls back to first_seen_at).
    assert_eq!(search(at("2026-02-01T00:00:00Z")).await, vec![h.vim]);
    // The end instant belongs to the successor.
    assert_eq!(search(at("2026-03-01T00:00:00Z")).await, vec![h.helix]);
    // Before the first sighting nothing held; the merged duplicate never does.
    assert!(search(at("2025-12-31T00:00:00Z")).await.is_empty());
    assert!(!search(at("2026-02-01T00:00:00Z")).await.contains(&h.dup));
}

#[sqlx::test(migrations = "../../migrations")]
async fn as_of_search_applies_to_every_tier(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let obs = insert_obs(&w, Obs::new(space, "Rotated the nginx certificate", "nginx certificate rotation")).await;
    let archived = insert_obs(&w, Obs::new(space, "Rotated the nginx cache", "nginx cache purge")).await;
    let summary = insert_summary(
        &w,
        space,
        &project(),
        None,
        SummaryGrain::Session,
        "nginx certificate work",
        "renew nginx certificate",
        1.0,
    )
    .await;
    let old_proc = insert_procedure(&w, space, "Renew nginx certificate", "deploy", "run certbot v1").await;
    let new_proc = insert_procedure(&w, space, "Renew nginx certificate", "deploy", "run certbot v2").await;
    for (id, when) in [(obs, "2026-02-01"), (archived, "2026-02-01"), (summary, "2026-02-02"), (old_proc, "2026-01-01")]
    {
        let table = if id == summary {
            "summaries"
        } else if id == old_proc {
            "procedures"
        } else {
            "observations"
        };
        sqlx::query(&format!("UPDATE {table} SET created_at = $2::timestamptz WHERE id = $1"))
            .bind(id)
            .bind(when)
            .execute(&w.pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE observations SET archived_at = '2026-04-01T00:00:00Z' WHERE id = $1")
        .bind(archived)
        .execute(&w.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE procedures SET created_at = '2026-03-01T00:00:00Z', supersedes_id = $2 WHERE id = $1")
        .bind(new_proc)
        .bind(old_proc)
        .execute(&w.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE procedures SET status = 'retired' WHERE id = $1")
        .bind(old_proc)
        .execute(&w.pool)
        .await
        .unwrap();
    let search = |as_of| found_as_of(&w, "nginx certificate", MemoryTable::ALL, as_of);
    let sorted = |mut v: Vec<Uuid>| {
        v.sort();
        v
    };
    // January: only the first procedure existed.
    assert_eq!(search(at("2026-01-15T00:00:00Z")).await, vec![old_proc]);
    // Mid February: the observations and the summary exist and the first procedure still holds.
    assert_eq!(search(at("2026-02-15T00:00:00Z")).await, sorted(vec![obs, archived, summary, old_proc]));
    // Mid March the new procedure replaced it; the observation archived in April is still there.
    assert_eq!(search(at("2026-03-15T00:00:00Z")).await, sorted(vec![obs, archived, summary, new_proc]));
    assert_eq!(search(at("2026-04-15T00:00:00Z")).await, sorted(vec![obs, summary, new_proc]));
    // Without as_of the archived observation and the retired procedure are gone.
    let now = search(None).await;
    assert!(now.contains(&obs) && now.contains(&new_proc) && !now.contains(&old_proc));
}

#[sqlx::test(migrations = "../../migrations")]
async fn as_of_recall_uses_the_past(pool: PgPool) {
    let w = world(pool).await;
    let h = insert_editor_history(&w).await;
    sqlx::query(
        "INSERT INTO profiles (space_id, scope_kind, scope_key, body_md, token_estimate, facts_version)
         VALUES ($1, 'global', '', '- Prefers helix [fact#abcdef · 0.80 · 2026-10-01]', 10, 1)",
    )
    .bind(w.alice.space_id)
    .execute(&w.pool)
    .await
    .unwrap();
    let recall = |as_of, profile| {
        let budgets = Budgets { profile, ..Budgets::default() };
        let r = RecallRequest { rerank: false, budgets, as_of, ..req("editor") };
        engine::recall(&w.state, &w.alice.identity, r)
    };
    let facts = |resp: &memvault_core::schemas::recall::RecallResponse| {
        resp.slots.facts.iter().map(|f| f.item.id).collect::<Vec<_>>()
    };
    assert_eq!(facts(&recall(None, 0).await.unwrap()), vec![h.helix]);
    assert_eq!(facts(&recall(at("2026-02-01T00:00:00Z"), 0).await.unwrap()), vec![h.vim]);
    // The rendered profile describes the present; a past recall rebuilds it from the facts valid then.
    let now = recall(None, Budgets::default().profile).await.unwrap();
    assert!(now.slots.profile.unwrap().body_md.contains("Prefers helix"));
    let past = recall(at("2026-02-01T00:00:00Z"), Budgets::default().profile).await.unwrap();
    let profile = past.slots.profile.unwrap().body_md;
    assert!(profile.contains("vim") && !profile.contains("helix"), "{profile}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn history_walks_the_chain_both_ways(pool: PgPool) {
    let w = world(pool).await;
    let h = insert_editor_history(&w).await;
    sqlx::query("UPDATE facts SET status = 'superseded', valid_to = '2026-05-01T00:00:00Z' WHERE id = $1")
        .bind(h.helix)
        .execute(&w.pool)
        .await
        .unwrap();
    let third = insert_fact(&w, Fact::new(w.alice.space_id, "prefers.editor", "zed")).await;
    sqlx::query("UPDATE facts SET valid_from = '2026-05-01T00:00:00Z', supersedes_id = $2 WHERE id = $1")
        .bind(third)
        .bind(h.helix)
        .execute(&w.pool)
        .await
        .unwrap();
    // The same chain from any link, oldest first, the asked-for one marked.
    for asked in [h.vim, h.helix, third] {
        let hist = engine::history(&w.state, &w.alice.identity, asked).await.unwrap();
        let ids: Vec<Uuid> = hist.entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![h.vim, h.helix, third], "from {asked}");
        assert_eq!(hist.entries.iter().filter(|e| e.current).map(|e| e.id).collect::<Vec<_>>(), vec![asked]);
    }
    let hist = engine::history(&w.state, &w.alice.identity, h.helix).await.unwrap();
    let [vim, helix, zed] = &hist.entries[..] else { panic!("{:?}", hist.entries) };
    assert_eq!((vim.text.as_str(), vim.status.as_str()), ("prefers.editor: vim", "superseded"));
    assert_eq!(vim.superseded_by, Some(h.helix));
    assert_eq!((vim.ended_by.as_deref(), vim.reason.as_deref()), (Some("planner"), Some("switched in March")));
    assert_eq!(vim.model.as_deref(), Some("sonnet"));
    assert_eq!(helix.supersedes_id, Some(h.vim));
    assert_eq!(helix.superseded_by, Some(third), "filled from the chain when the column is empty");
    assert!(helix.ended_by.is_none(), "no audit entry is known for it");
    assert_eq!((zed.status.as_str(), zed.valid_to, zed.superseded_by), ("active", None, None));
    // The merged duplicate is no link of the chain; asked for directly it stands alone.
    let dup = engine::history(&w.state, &w.alice.identity, h.dup).await.unwrap();
    assert_eq!(dup.entries.len(), 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn history_covers_procedures_and_single_memories(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let v1 = insert_procedure(&w, space, "Deploy the site", "deploy", "rsync the build").await;
    let v2 = insert_procedure(&w, space, "Deploy the site", "deploy", "run the pipeline").await;
    sqlx::query("UPDATE procedures SET created_at = '2026-02-01T00:00:00Z' WHERE id = $1")
        .bind(v1)
        .execute(&w.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE procedures SET status = 'retired' WHERE id = $1").bind(v1).execute(&w.pool).await.unwrap();
    sqlx::query("UPDATE procedures SET supersedes_id = $2, version = 2 WHERE id = $1")
        .bind(v2)
        .bind(v1)
        .execute(&w.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memory_audit (id, space_id, actor, action, target_table, target_id, reason, created_at)
         VALUES ($1, $2, 'planner', 'proc.retire', 'procedures', $3, 'revised by a newer version',
                 '2026-04-01T00:00:00Z')",
    )
    .bind(Uuid::now_v7())
    .bind(space)
    .bind(v1)
    .execute(&w.pool)
    .await
    .unwrap();
    let hist = engine::history(&w.state, &w.alice.identity, v2).await.unwrap();
    let [first, second] = &hist.entries[..] else { panic!("{:?}", hist.entries) };
    assert_eq!((first.id, second.id), (v1, v2));
    assert!(second.text.starts_with("Deploy the site (v2): run the pipeline"), "{}", second.text);
    assert_eq!((first.status.as_str(), first.superseded_by), ("retired", Some(v2)));
    assert_eq!(first.valid_to, at("2026-04-01T00:00:00Z"), "the audited retirement ends it");
    assert_eq!(first.ended_by.as_deref(), Some("planner"));

    let obs = insert_obs(&w, Obs::new(space, "Plain note", "no chain")).await;
    let hist = engine::history(&w.state, &w.alice.identity, obs).await.unwrap();
    assert_eq!(hist.entries.len(), 1);
    assert!(hist.entries[0].current && hist.entries[0].status == "active");
}

#[sqlx::test(migrations = "../../migrations")]
async fn history_respects_rls_and_hides_other_secrets(pool: PgPool) {
    let w = world(pool).await;
    let bob = seed_identity(&w.pool, TokenKind::Device).await;
    let h = insert_editor_history(&w).await;
    let err = engine::history(&w.state, &bob.identity, h.vim).await.unwrap_err();
    assert_eq!((err.status.as_u16(), err.code.as_str()), (404, "not_found"));
    sqlx::query("UPDATE facts SET sensitivity = 'secret' WHERE id = $1").bind(h.vim).execute(&w.pool).await.unwrap();
    let from_helix = engine::history(&w.state, &w.alice.identity, h.helix).await.unwrap();
    assert_eq!(from_helix.entries.iter().map(|e| e.id).collect::<Vec<_>>(), vec![h.helix]);
    let from_vim = engine::history(&w.state, &w.alice.identity, h.vim).await.unwrap();
    assert_eq!(from_vim.entries.iter().map(|e| e.id).collect::<Vec<_>>(), vec![h.vim, h.helix]);
}
