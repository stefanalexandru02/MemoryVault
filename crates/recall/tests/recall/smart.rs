//! Smart injection: the memories a session's first prompt matches, or nothing.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{Method, StatusCode, header};
use memvault_core::estimate_tokens;
use memvault_core::schemas::recall::PromptContextRequest;
use memvault_core::tei::{Embedder, FakeTei, TeiError, TeiHealth};
use memvault_core::test_support::{app, send};
use memvault_recall::render::SMART_HEADER;
use memvault_recall::smart::{self, SMART_ROWS, SMART_TOKENS};
use serde_json::json;
use sqlx::PgPool;

use crate::common::*;

fn request(prompt: &str) -> PromptContextRequest {
    PromptContextRequest { prompt: prompt.into(), project_key: Some(PROJECT.into()), device_id: None }
}

async fn seed(w: &World) {
    let space = w.alice.space_id;
    for (title, narrative) in [
        ("Webhook retries come from the payment provider every 15 minutes", "retries stop after 24 hours"),
        ("Added idempotency table for webhook events", "dedupe on the provider event id"),
        ("Store money as integer minor units end to end", "no floats anywhere in the ledger"),
    ] {
        insert_obs(w, Obs::new(space, title, narrative)).await;
    }
}

async fn smart_rows(w: &World) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM retrieval_log WHERE mode = 'smart'").fetch_one(&w.pool).await.unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_prompt_that_shares_terms_with_memories_gets_their_index_rows(pool: PgPool) {
    let w = world(pool).await;
    seed(&w).await;
    let prompt = "why does the payment provider keep sending webhook retries for the same event?";
    let md = smart::prompt_context(&w.state, &w.alice.identity, request(prompt)).await.unwrap();
    assert!(md.starts_with(SMART_HEADER), "{md}");
    assert!(md.contains("Webhook retries come from the payment provider") && md.contains("idempotency table"), "{md}");
    assert!(!md.contains("integer minor units"), "unrelated memory left out:\n{md}");
    assert!(md.contains("| id | time | t | title | ~tok |") && md.contains("\n_Legend: "), "{md}");
    assert!(md.lines().any(|l| l.starts_with("**20")), "rows are grouped by day: {md}");
    assert!(estimate_tokens(&md) <= SMART_TOKENS);

    // Logged as a smart injection, with what it cost and how many rows it showed.
    let (mode, injection, tokens, hits, query): (String, String, i32, i32, String) =
        sqlx::query_as("SELECT mode, injection, tokens, hits, query FROM retrieval_log WHERE mode = 'smart'")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!((mode.as_str(), injection.as_str(), tokens, hits), ("smart", "smart", estimate_tokens(&md), 2));
    assert_eq!(query, prompt);
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_unrelated_or_thin_prompt_gets_nothing_and_leaves_no_log(pool: PgPool) {
    let w = world(pool).await;
    seed(&w).await;
    for prompt in [
        "what is the weather like in Lisbon tomorrow afternoon",
        // One word in common is not enough without a reranker to vouch for it.
        "webhook",
        "fix the build",
        "the and of it",
    ] {
        let md = smart::prompt_context(&w.state, &w.alice.identity, request(prompt)).await.unwrap();
        assert_eq!(md, "", "{prompt}");
    }
    assert_eq!(smart_rows(&w).await, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn many_matches_are_capped_in_rows_and_tokens(pool: PgPool) {
    let w = world(pool).await;
    let words: Vec<String> = (0..60).map(|i| format!("word{i}")).collect();
    // Twelve distinct notes on the topic, each with a long title of its own: five rows would not fit 250 tokens.
    for i in 0..12 {
        let filler = (0..20).map(|k| words[(i * 5 + k * 7) % 60].as_str()).collect::<Vec<_>>().join(" ");
        insert_obs(&w, Obs::new(w.alice.space_id, &format!("Ledger export rounding {filler}"), "n")).await;
    }
    let md =
        smart::prompt_context(&w.state, &w.alice.identity, request("ledger export fails on rounding")).await.unwrap();
    let rows = md.lines().filter(|l| l.starts_with("| ") && !l.starts_with("| id ")).count();
    assert!((2..SMART_ROWS).contains(&rows), "the token cap trims below {SMART_ROWS} rows, {rows} shown:\n{md}");
    assert!(estimate_tokens(&md) <= SMART_TOKENS, "{} tokens:\n{md}", estimate_tokens(&md));
    let (tokens, hits): (i32, i32) =
        sqlx::query_as("SELECT tokens, hits FROM retrieval_log WHERE mode = 'smart'").fetch_one(&w.pool).await.unwrap();
    assert_eq!((tokens, hits as usize), (estimate_tokens(&md), rows));
}

/// A cross-encoder stand-in: 0.9 for texts about the topic, 0.05 for the rest.
struct Calibrated {
    inner: FakeTei,
    down: bool,
}

#[async_trait]
impl Embedder for Calibrated {
    fn model(&self) -> &str {
        self.inner.model()
    }
    fn dims(&self) -> usize {
        self.inner.dims()
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, TeiError> {
        self.inner.embed(texts).await
    }
    async fn rerank(&self, _query: &str, texts: &[String]) -> Result<Vec<f32>, TeiError> {
        if self.down {
            return Err(TeiError::Failed("reranker down".into()));
        }
        Ok(texts.iter().map(|t| if t.contains("ledger") { 0.9 } else { 0.05 }).collect())
    }
    async fn health(&self) -> TeiHealth {
        self.inner.health().await
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn with_a_cross_encoder_its_score_decides_and_without_one_the_overlap_rule_does(pool: PgPool) {
    let mut w = world(pool).await;
    insert_obs(&w, Obs::new(w.alice.space_id, "Money is kept as integer cents", "the ledger never sees floats")).await;
    insert_obs(&w, Obs::new(w.alice.space_id, "Staging deploy moved to Fly", "region fra")).await;
    // No word of the prompt is in the memory: only a model could connect them.
    let prompt = "how do we represent currency amounts";
    w.state.tei = Arc::new(Calibrated { inner: FakeTei::default(), down: false });
    let md = smart::prompt_context(&w.state, &w.alice.identity, request(prompt)).await.unwrap();
    assert!(md.contains("integer cents") && !md.contains("Staging deploy"), "reranked above 0.5:\n{md}");
    let before = smart_rows(&w).await;

    // The same prompt when the reranker is down: the overlap rule applies, and nothing overlaps.
    w.state.tei = Arc::new(Calibrated { inner: FakeTei::default(), down: true });
    assert_eq!(smart::prompt_context(&w.state, &w.alice.identity, request(prompt)).await.unwrap(), "");
    assert_eq!(smart_rows(&w).await, before);
    // A prompt that shares two words with a memory still works then.
    let md = smart::prompt_context(&w.state, &w.alice.identity, request("does the ledger see floats anywhere"))
        .await
        .unwrap();
    assert!(md.contains("integer cents"), "{md}");

    // The threshold is a setting.
    w.state.tei = Arc::new(Calibrated { inner: FakeTei::default(), down: false });
    let mut settings = (*w.state.settings).clone();
    settings.smart_rerank_min = 0.95;
    w.state.settings = Arc::new(settings);
    assert_eq!(smart::prompt_context(&w.state, &w.alice.identity, request(prompt)).await.unwrap(), "");
}

/// Embeds like FakeTei, but the lookup takes a second.
struct Slow(FakeTei);

#[async_trait]
impl Embedder for Slow {
    fn model(&self) -> &str {
        self.0.model()
    }
    fn dims(&self) -> usize {
        self.0.dims()
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, TeiError> {
        tokio::time::sleep(Duration::from_secs(1)).await;
        self.0.embed(texts).await
    }
    async fn rerank(&self, query: &str, texts: &[String]) -> Result<Vec<f32>, TeiError> {
        self.0.rerank(query, texts).await
    }
    async fn health(&self) -> TeiHealth {
        self.0.health().await
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_slow_lookup_is_an_empty_answer_within_the_budget(pool: PgPool) {
    let mut w = world(pool).await;
    seed(&w).await;
    w.state.tei = Arc::new(Slow(FakeTei::default()));
    let started = std::time::Instant::now();
    let md =
        smart::prompt_context(&w.state, &w.alice.identity, request("payment provider webhook retries")).await.unwrap();
    assert_eq!(md, "");
    assert!(started.elapsed() < Duration::from_millis(900), "{:?}", started.elapsed());
    assert_eq!(smart_rows(&w).await, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn subagent_secret_and_other_project_memories_are_not_offered(pool: PgPool) {
    let w = world(pool).await;
    let space = w.alice.space_id;
    let mut sub = Obs::new(space, "Webhook retries explored by a subagent", "payment provider retries");
    sub.entities = vec!["subagent:Explore".into()];
    insert_obs(&w, sub).await;
    let mut secret = Obs::new(space, "Webhook retries secret key", "payment provider retries key");
    secret.sensitivity = memvault_core::schemas::common::Sensitivity::Secret;
    insert_obs(&w, secret).await;
    let mut other = Obs::new(space, "Webhook retries in another project", "payment provider retries");
    other.scope = memvault_core::schemas::common::Scope::new(
        memvault_core::schemas::common::ScopeKind::Project,
        "github.com/x/y",
    );
    insert_obs(&w, other).await;
    let md =
        smart::prompt_context(&w.state, &w.alice.identity, request("payment provider webhook retries")).await.unwrap();
    assert_eq!(md, "");
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_route_answers_markdown_or_an_empty_body_and_stats_count_the_injections(pool: PgPool) {
    let w = world(pool).await;
    seed(&w).await;
    let app = app(w.state.clone(), memvault_recall::router());
    let token = w.alice.token.as_str();
    let post = |prompt: &str| {
        let body = json!({"prompt": prompt, "project_key": PROJECT});
        send(&app, Method::POST, "/v1/context/prompt", Some(token), Some(body))
    };

    let hit = post("payment provider webhook retries repeat").await;
    assert_eq!(hit.status, StatusCode::OK, "{}", hit.text());
    assert_eq!(hit.headers[header::CONTENT_TYPE], "text/markdown; charset=utf-8");
    assert!(hit.text().starts_with(SMART_HEADER));
    let miss = post("weather in Lisbon tomorrow").await;
    assert_eq!((miss.status, miss.text().as_str()), (StatusCode::OK, ""));
    for bad in [json!({"prompt": "  "}), json!({})] {
        let resp = send(&app, Method::POST, "/v1/context/prompt", Some(token), Some(bad)).await;
        assert_eq!(resp.status, 422);
    }
    assert_eq!(send(&app, Method::POST, "/v1/context/prompt", None, Some(json!({"prompt": "x y"}))).await.status, 401);

    // The context route says which mode it rendered, for clients that act on `smart`; a resume has no block.
    let ctx =
        send(&app, Method::GET, &format!("/v1/context?project_key={PROJECT}&mode=smart"), Some(token), None).await;
    assert_eq!(ctx.headers["x-memory-injection"], "smart");
    assert!(ctx.text().ends_with(memvault_recall::render::LEAN_RUBRIC), "smart starts with the lean block");
    let resume = send(&app, Method::GET, "/v1/context?source=resume", Some(token), None).await;
    assert!(resume.headers.get("x-memory-injection").is_none());

    let stats = send(&app, Method::GET, "/v1/stats/recall", Some(token), None).await.json();
    let smart_stats: Vec<_> = stats["modes"].as_array().unwrap().iter().filter(|m| m["mode"] == "smart").collect();
    assert_eq!(smart_stats.len(), 1, "{stats}");
    assert_eq!((smart_stats[0]["injection"].clone(), smart_stats[0]["calls"].clone()), (json!("smart"), json!(1)));
    assert!(
        smart_stats[0]["avg_tokens"].as_f64().unwrap() > 0.0 && smart_stats[0]["avg_hits"].as_f64().unwrap() >= 1.0
    );
}
