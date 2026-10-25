//! Smart injection (`POST /v1/context/prompt`): the first prompt of a session, answered with the few observations it
//! matches, or with nothing.
//!
//! Relevance has no scale of its own: RRF scores depend on the query, so they cannot be held against a threshold.
//! With a cross-encoder (the reranker answered, and it is a real one) an observation must score at least
//! `SMART_RERANK_MIN`, a probability-like number that means the same for every prompt. Without one (`FAKE_TEI`, the
//! reranker down or too slow) it must share at least [`MIN_OVERLAP`] distinct non-stopword terms of the prompt with
//! its title, subtitle and narrative, which is what the keyword leg matches on; a prompt of one meaningful word then
//! finds nothing, which is the safe answer. At most [`SMART_ROWS`] rows come back, in at most [`SMART_TOKENS`]
//! estimated tokens, within [`BUDGET`]; a slow or failing lookup is an empty answer, never an error to the prompt.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use memvault_core::auth::resolve_read_spaces;
use memvault_core::db::{caps, tune_vector_search};
use memvault_core::schemas::common::{InjectionMode, MemoryTable, Sensitivity};
use memvault_core::schemas::recall::PromptContextRequest;
use memvault_core::{ApiError, AppState, Identity, estimate_tokens, user_tx};
use serde_json::json;
use uuid::Uuid;

use crate::candidates::{
    Filters, KEYWORD_LIMIT, ObsRow, TierRow, VECTOR_LIMIT, hydrate, keyword_leg, query_terms, vector_leg,
};
use crate::context::context_scopes;
use crate::engine::{RetrievalRecord, ef_search_for, embed_query, index_row, insert_retrieval, rerank};
use crate::render::{self, IndexEntry};
use crate::scoring::{Candidate, diversify, rrf_fuse};

pub const SMART_ROWS: usize = 5;
pub const SMART_TOKENS: i32 = 250;
/// Characters of the prompt that count.
pub const PROMPT_CHARS: usize = 500;
/// Distinct prompt terms an observation must contain when no calibrated reranker is there to judge it.
pub const MIN_OVERLAP: usize = 2;
/// The whole lookup, a little under the hook's 800 ms.
const BUDGET: Duration = Duration::from_millis(600);
/// Candidates fused and sent to the reranker.
const FUSED: usize = 20;

/// Number of the prompt's terms (see [`query_terms`]) among the words of the observation's title, subtitle and
/// narrative.
fn overlap(terms: &[String], row: &ObsRow) -> usize {
    let text =
        format!("{} {} {}", row.title, row.subtitle.as_deref().unwrap_or(""), row.narrative.as_deref().unwrap_or(""));
    let text = text.to_lowercase();
    let words: HashSet<&str> = text.split(|c: char| !(c.is_alphanumeric() || c == '_')).collect();
    terms.iter().filter(|t| words.contains(t.as_str())).count()
}

/// How well a candidate answers the prompt, if well enough to show: with `rerank_min` its reranker score when at
/// least that, otherwise the terms it shares with the prompt when at least [`MIN_OVERLAP`] (plus its fused rank, which
/// is below 1, to order equals).
fn rank(c: &Candidate, terms: &[String], rerank_min: Option<f64>) -> Option<f64> {
    let Some(TierRow::Obs(row)) = &c.row else { return None };
    match rerank_min {
        Some(min) => c.rerank.filter(|score| *score >= min),
        None => Some(overlap(terms, row)).filter(|n| *n >= MIN_OVERLAP).map(|n| n as f64 + c.rrf),
    }
}

/// The candidates worth showing, best first (see [`rank`]).
pub fn relevant(cands: &[Candidate], terms: &[String], rerank_min: Option<f64>) -> Vec<Candidate> {
    let mut kept: Vec<(f64, &Candidate)> =
        cands.iter().filter_map(|c| rank(c, terms, rerank_min).map(|r| (r, c))).collect();
    kept.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
    kept.into_iter().map(|(_, c)| c.clone()).collect()
}

/// The block for these candidates: up to [`SMART_ROWS`] distinct rows, the last ones dropped until it fits
/// [`SMART_TOKENS`]; empty for none.
pub fn block(ordered: Vec<Candidate>) -> (String, Vec<Candidate>) {
    let mut chosen = diversify(ordered, SMART_ROWS);
    loop {
        let entries: Vec<IndexEntry> = chosen
            .iter()
            .filter_map(|c| c.row.as_ref())
            .map(|row| IndexEntry::from_row(&index_row(row, None, false)))
            .collect();
        let text = render::smart_block(&entries);
        if estimate_tokens(&text) <= SMART_TOKENS {
            return (text, chosen);
        }
        if chosen.pop().is_none() {
            return (String::new(), chosen);
        }
    }
}

/// The memories the prompt matches as markdown, or an empty string. Logged in `retrieval_log` (mode `smart`) only
/// when something is returned; `memsvc_smart_context_total{outcome}` counts every call.
pub async fn prompt_context(
    state: &AppState,
    identity: &Identity,
    req: PromptContextRequest,
) -> Result<String, ApiError> {
    let prompt: String = render::one_line(&req.prompt).chars().take(PROMPT_CHARS).collect();
    let (outcome, text) = match tokio::time::timeout(BUDGET, answer(state, identity, &req, &prompt)).await {
        Ok(Ok(text)) if text.is_empty() => ("nothing", text),
        Ok(Ok(text)) => ("injected", text),
        Ok(Err(e)) => return Err(e),
        Err(_) => ("timeout", String::new()),
    };
    metrics::counter!("memsvc_smart_context_total", "outcome" => outcome).increment(1);
    Ok(text)
}

async fn answer(
    state: &AppState,
    identity: &Identity,
    req: &PromptContextRequest,
    prompt: &str,
) -> Result<String, ApiError> {
    let started = Instant::now();
    let terms = query_terms(prompt);
    if terms.is_empty() {
        return Ok(String::new());
    }
    let mut tx = user_tx(&state.db.app, identity).await?;
    let spaces = resolve_read_spaces(&mut tx, identity, None).await?;
    if spaces.is_empty() {
        return Ok(String::new());
    }
    let project_key = req.project_key.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let device = req.device_id.as_deref().map(str::trim).filter(|d| !d.is_empty()).unwrap_or(&identity.device_id);
    let scopes = context_scopes(project_key, device);
    let filters = Filters {
        spaces: spaces.clone(),
        scopes: Some(scopes.clone()),
        exclude_sensitivity: vec![Sensitivity::Secret],
        exclude_subagent: true,
        ..Filters::default()
    };
    let table = MemoryTable::Observations;
    let db_caps = caps(&state.db.app).await?;
    let kw = keyword_leg(&mut tx, db_caps.pg_search, table, &filters, &terms, &[], KEYWORD_LIMIT).await?;
    let vec = match embed_query(state, prompt).await {
        Some(v) => {
            tune_vector_search(&mut tx, &db_caps, ef_search_for(VECTOR_LIMIT)).await?;
            vector_leg(&mut tx, table, &filters, &v, state.tei.model(), true, VECTOR_LIMIT).await?
        }
        None => Vec::new(),
    };
    let mut cands = rrf_fuse(table, &kw, &vec, FUSED);
    let ids: Vec<Uuid> = cands.iter().map(|c| c.id).collect();
    let mut rows = hydrate(&mut tx, table, &ids).await?;
    cands.retain_mut(|c| {
        c.row = rows.remove(&c.id);
        c.row.is_some()
    });
    let reranked = rerank(state, prompt, &mut cands.iter_mut().collect::<Vec<_>>()).await;
    let rerank_min = (reranked && state.tei.calibrated_rerank()).then_some(state.settings.smart_rerank_min);
    let (text, shown) = block(relevant(&cands, &terms, rerank_min));
    if shown.is_empty() {
        return Ok(String::new());
    }
    let rec = RetrievalRecord {
        mode: "smart",
        query: Some(prompt),
        spaces: &spaces,
        scopes: &scopes,
        budgets: None,
        latency_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
        injection: Some(InjectionMode::Smart),
        tokens: estimate_tokens(&text),
        hits: i32::try_from(shown.len()).unwrap_or(i32::MAX),
    };
    let candidates: Vec<_> = cands.iter().map(Candidate::log).collect();
    let selected: Vec<_> = shown.iter().map(|c| json!({ "id": c.id, "slot": "observations" })).collect();
    insert_retrieval(&mut tx, identity, rec, json!(candidates), json!(selected), reranked).await?;
    tx.commit().await?;
    Ok(text)
}
