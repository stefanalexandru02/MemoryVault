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

/// The full SessionStart block, with 250 observations behind it.
const FULL_BLOCK_TOKENS: i32 = 1_000;
/// The lean block: profile, open work and the tool rubric.
const LEAN_BLOCK_TOKENS: i32 = 250;
/// Five rows of `memory_search`.
const SEARCH_5_ROWS_TOKENS: i32 = 170;

const TITLES: [&str; 5] = [
    "Invoice totals double-counted tax on discounted line items in module",
    "Store money as integer minor units end to end in module",
    "Staging database URL was logged at debug level in module",
    "Webhook retries come from the payment provider every 15 minutes for 24 hours in module",
    "Added idempotency table for webhook events in module",
];
const TYPES: [&str; 6] = ["decision", "bugfix", "feature", "refactor", "change", "discovery"];

async fn seed(w: &World) {
    let space = w.alice.space_id;
    for i in 0..250_usize {
        let mut o = Obs::new(space, &format!("{} {}", TITLES[i % TITLES.len()], i / TITLES.len() + 1), "narrative");
        o.obs_type = TYPES[i % TYPES.len()];
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

#[sqlx::test(migrations = "../../migrations")]
async fn session_start_blocks_stay_within_budget(pool: PgPool) {
    let w = world(pool).await;
    seed(&w).await;
    let (full, lean) = (block(&w, InjectionMode::Full).await, block(&w, InjectionMode::Lean).await);
    let (full_tokens, lean_tokens) = (estimate_tokens(&full), estimate_tokens(&lean));
    assert!(full_tokens < FULL_BLOCK_TOKENS, "full block is {full_tokens} estimated tokens:\n{full}");
    assert!(lean_tokens < LEAN_BLOCK_TOKENS, "lean block is {lean_tokens} estimated tokens:\n{lean}");
    // A real index, not an empty one: the guard is about cost per row.
    assert!(full.matches("\n| ").count() >= 20, "{full}");
    assert!(full.contains("### Since you were here") && full.contains("### Relevant procedures"), "{full}");
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
