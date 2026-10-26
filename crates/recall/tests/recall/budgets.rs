//! Context cost guards: what MemoryVault puts into a model's context may not grow silently. The fixture is a
//! project with ~250 observations, four profile lines, a procedure and a session on another device, much like a
//! few weeks of real use. Sizes are `estimate_tokens`, the estimator the server itself budgets with.

use memvault_core::auth::admin::ensure_client;
use memvault_core::schemas::common::{ClientKind, InjectionMode, SummaryGrain};
use memvault_core::schemas::memories::SearchQuery;
use memvault_core::{estimate_tokens, system_tx};
use memvault_recall::{context, engine, render};
use serde_json::json;
use sqlx::PgPool;

use crate::common::*;

/// The full SessionStart block, with 250 observations behind it and the default 12 index rows (the 28 rows it showed
/// before cost about 980 on the same kind of project).
const FULL_BLOCK_TOKENS: i32 = 560;
/// What one more index row adds, at most (the fixture's titles are 5-10 words, like the observer's).
const INDEX_ROW_TOKENS: i32 = 22;
/// The lean block: profile, open work and the tool rubric.
const LEAN_BLOCK_TOKENS: i32 = 250;
/// Five rows of `memory_search`.
const SEARCH_5_ROWS_TOKENS: i32 = 170;

const VERBS: [&str; 10] =
    ["Fixed", "Added", "Removed", "Renamed", "Cached", "Retried", "Validated", "Split", "Logged", "Batched"];
const WHAT: [&str; 10] = [
    "invoice totals",
    "webhook retries",
    "the staging database URL",
    "money amounts",
    "idempotency keys",
    "the ledger export",
    "tax rounding",
    "customer emails",
    "the payment provider client",
    "audit log entries",
];
const WHERE: [&str; 10] = [
    "discounted line items",
    "the billing module",
    "the nightly job",
    "the checkout flow",
    "the admin dashboard",
    "debug builds",
    "the retry queue",
    "the CSV importer",
    "the signup page",
    "test fixtures",
];
const TYPES: [&str; 6] = ["decision", "bugfix", "feature", "refactor", "change", "discovery"];

/// Title `i` of 250 distinct observations: a verb, a thing and a place, each of the 1,000 combinations at most
/// once, so neighbours share a word or two but are not the same note worded twice.
fn title(i: usize) -> String {
    let n = i * 37 % 1000;
    format!("{} {} in {}", VERBS[n % 10], WHAT[n / 10 % 10], WHERE[n / 100])
}

async fn seed(w: &World) {
    let space = w.alice.space_id;
    for i in 0..250_usize {
        let mut o = Obs::new(space, &title(i), "narrative");
        o.obs_type = TYPES[i % TYPES.len()];
        o.importance = 0.3 + 0.1 * (i % 7) as f32;
        o.age_days = 0.1 + i as f64 * 0.045;
        o.embed = false;
        insert_obs(w, o).await;
    }
    for (scope_kind, scope_key, body) in [
        (
            "global",
            "",
            "- user: prefers.editor = Neovim with the LazyVim distribution [fact#6ff3f5]\n\
             - user: prefers.commit_style = Conventional commits, imperative mood, no trailing period [fact#02085c]",
        ),
        (
            "project",
            PROJECT,
            "- homelab: deploys.target = Fly.io, region fra, via GitHub Actions on tag push [fact#32b539]\n\
             - homelab: database.engine = PostgreSQL 16 [fact#9eedcf]",
        ),
    ] {
        sqlx::query(
            "INSERT INTO profiles (space_id, scope_kind, scope_key, body_md, token_estimate, facts_version)
             VALUES ($1, $2::scope_kind, $3, $4, 40, 1)",
        )
        .bind(space)
        .bind(scope_kind)
        .bind(scope_key)
        .bind(body)
        .execute(&w.pool)
        .await
        .unwrap();
    }
    insert_procedure(
        w,
        space,
        "Cut a release",
        "release",
        "1. Make sure main is green. 2. Bump the version in Cargo.toml and update CHANGELOG.md. 3. Tag with `vX.Y.Z` \
         and push the tag. 4. Watch the deploy workflow; roll back by redeploying the previous tag.",
    )
    .await;
    insert_work_state(&w.pool, space, &project(), "todo", json!({"backfill": "open", "ledger job": "open"})).await;

    // This device was here two days ago and asks now; another one worked in between.
    insert_session(&w.pool, space, w.alice.client_id, PROJECT, 2.0 * 24.0 * 60.0).await;
    insert_session(&w.pool, space, w.alice.client_id, PROJECT, 1.0).await;
    let mut tx = system_tx(&w.pool).await.unwrap();
    let desktop = ensure_client(&mut tx, w.alice.user_id, "desktop", ClientKind::ClaudeCode, None, None).await.unwrap();
    tx.commit().await.unwrap();
    let elsewhere = insert_session(&w.pool, space, desktop, PROJECT, 24.0 * 60.0).await;
    insert_summary(
        w,
        space,
        &project(),
        Some(elsewhere),
        SummaryGrain::Session,
        "Fix the invoice total mismatch reported by finance",
        "Backfill the 41 affected invoices. Add a ledger reconciliation job to CI.",
        1.0,
    )
    .await;
    insert_session(&w.pool, space, desktop, PROJECT, 3.0 * 24.0 * 60.0).await;
}

async fn block(w: &World, mode: InjectionMode) -> String {
    context::render_context(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", Some(mode)).await.unwrap()
}

async fn block_with_rows(w: &World, rows: u32) -> String {
    let options = context::ContextOptions {
        mode: Some(InjectionMode::Full),
        rows: Some(rows),
        types: Vec::new(),
        rerender: false,
    };
    context::render_context_with(&w.state, &w.alice.identity, Some(PROJECT), None, "startup", &options)
        .await
        .unwrap()
        .body
}

fn index_rows(block: &str) -> usize {
    block.lines().filter(|l| l.starts_with("| ") && !l.starts_with("| id ")).count()
}

#[sqlx::test(migrations = "../../migrations")]
async fn session_start_blocks_stay_within_budget(pool: PgPool) {
    let w = world(pool).await;
    seed(&w).await;
    let (full, lean) = (block(&w, InjectionMode::Full).await, block(&w, InjectionMode::Lean).await);
    let (full_tokens, lean_tokens) = (estimate_tokens(&full), estimate_tokens(&lean));
    assert!(full_tokens < FULL_BLOCK_TOKENS, "full block is {full_tokens} estimated tokens:\n{full}");
    assert!(lean_tokens < LEAN_BLOCK_TOKENS, "lean block is {lean_tokens} estimated tokens:\n{lean}");
    // A real index, not an empty one: the guard is about cost per row.
    assert_eq!(index_rows(&full), 12, "{full}");
    assert!(full.contains("### Since you were here") && full.contains("### Relevant procedures"), "{full}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn index_rows_are_distinct_mixed_and_cost_a_few_tokens_each(pool: PgPool) {
    let w = world(pool).await;
    seed(&w).await;
    let (six, twelve, thirty) =
        (block_with_rows(&w, 6).await, block_with_rows(&w, 12).await, block_with_rows(&w, 30).await);
    assert_eq!([index_rows(&six), index_rows(&twelve), index_rows(&thirty)], [6, 12, 30]);
    let tokens = |b: &str| estimate_tokens(b);
    assert!(tokens(&twelve) - tokens(&six) <= 6 * INDEX_ROW_TOKENS, "{} vs {}", tokens(&twelve), tokens(&six));
    assert!(tokens(&thirty) - tokens(&twelve) <= 18 * INDEX_ROW_TOKENS, "{} vs {}", tokens(&thirty), tokens(&twelve));
    // No title appears twice, and no type fills more than a third of the rows (the fixture has six types).
    let titles: Vec<&str> = twelve.lines().filter(|l| l.starts_with("| ") && !l.starts_with("| id ")).collect();
    let mut seen: Vec<&str> = titles.iter().filter_map(|l| l.split(" | ").nth(3)).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 12, "{twelve}");
    for glyph in ["⚖", "●", "◆", "↻", "✓", "○"] {
        let n = titles.iter().filter(|l| l.contains(&format!(" | {glyph} | "))).count();
        assert!(n <= 4, "{n} rows of {glyph}:\n{twelve}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn five_search_rows_stay_within_budget(pool: PgPool) {
    let w = world(pool).await;
    seed(&w).await;
    let query = SearchQuery { q: Some("webhook idempotency".into()), limit: 5, ..SearchQuery::default() };
    let rows = engine::search(&w.state, &w.alice.identity, query).await.unwrap().rows;
    assert_eq!(rows.len(), 5);
    let table = render::search_rows_markdown(&rows);
    let tokens = estimate_tokens(&table);
    assert!(tokens < SEARCH_5_ROWS_TOKENS, "5 search rows are {tokens} estimated tokens:\n{table}");
    assert!(!table.contains("Legend") && !table.contains("Next:"), "{table}");
}
