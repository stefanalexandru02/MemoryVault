//! Entry points shared with memvault-memories (MCP tools). Each opens its own `user_tx` for the
//! caller, so RLS bounds what is visible, and resolves space ids with
//! `memvault_core::auth::resolve_read_spaces` (403 on a foreign space).
//!
//! Pipeline ([`build_pack`]): per tier, keyword leg + vector leg (or the no-query leg) -> RRF ->
//! hydrate -> optional cross-tier rerank of the fused top 40 (250 ms budget, falls back to RRF) ->
//! final score (11.4) -> MMR within the tier -> greedy budget fill per slot (11.5) -> render.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use chrono::{DateTime, Utc};
use memvault_core::auth::resolve_read_spaces;
use memvault_core::db::caps;
use memvault_core::ids::new_id;
use memvault_core::schemas::common::{MemoryTable, Scope, ScopeKind, Sensitivity, SummaryGrain};
use memvault_core::schemas::memories::{
    AuditEntry, ByFileQuery, EntityRef, EvidenceObservation, ExplainResponse, FactFull, GetResponse, IndexRow,
    MAX_GET_IDS, MemoryItem, ObservationFull, ProcedureFull, ProcedureStatus, Provenance, SearchQuery, SearchResponse,
    SessionInfo, SummaryFull, TimelineQuery, TimelineResponse,
};
use memvault_core::schemas::recall::{
    Budgets, FactItem, FeedbackRequest, FeedbackResponse, ObservationItem, ProcedureItem, ProfileSlot, RecallFormat,
    RecallRequest, RecallResponse, RecallSlots, SlotItem, SummaryItem, TokenUsage, WorkStateList,
};
use memvault_core::{ApiError, AppState, Identity, estimate_tokens};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::candidates::{
    Filters, KEYWORD_LIMIT, TierRow, VECTOR_LIMIT, VECTOR_PART_FANOUT, best_part_embeddings, hydrate, keyword_leg,
    query_terms, recent_leg, vector_leg,
};
use crate::render;
use crate::scoring::{Candidate, budget_fill, by_score, mmr_order, rrf_fuse};
use memvault_core::db::tune_vector_search;
use memvault_core::user_tx;

/// Tier order of candidate generation and of `selected` in `retrieval_log`.
const TIER_ORDER: [MemoryTable; 4] =
    [MemoryTable::Facts, MemoryTable::Procedures, MemoryTable::Observations, MemoryTable::Summaries];
/// Fused candidates kept per tier (11.2 `LIMIT 40`) and sent to the reranker across tiers (11.3).
const FUSED_LIMIT: usize = 40;
const RERANK_TOP: usize = 40;
const NO_QUERY_LIMIT: i64 = 40;
const NO_QUERY_OBS_LIMIT: i64 = 60;
const PINNED_LIMIT: i64 = 20;
const MAX_LOGGED_CANDIDATES: usize = 200;
const EF_SEARCH: u32 = 80;
/// pgvector's upper bound for `hnsw.ef_search`.
const MAX_EF_SEARCH: i64 = 1000;

/// Without iterative scans (pgvector < 0.8) an HNSW scan yields at most `ef_search` tuples before the
/// space/tier/model filters apply, so it must cover the vector leg's inner `LIMIT`.
pub fn ef_search_for(vector_limit: i64) -> u32 {
    u32::try_from((vector_limit * VECTOR_PART_FANOUT).clamp(i64::from(EF_SEARCH), MAX_EF_SEARCH)).unwrap_or(EF_SEARCH)
}
const RECALL_SUMMARY_GRAINS: [SummaryGrain; 3] = [SummaryGrain::Session, SummaryGrain::Day, SummaryGrain::Week];

// ---------- scopes ----------

/// Deduplicated scopes in request order; 422 for a global scope with a key or another without one.
pub fn validate_scopes(scopes: &[Scope]) -> Result<Vec<Scope>, ApiError> {
    let mut out: Vec<Scope> = Vec::new();
    for s in scopes {
        if s.kind == ScopeKind::Global && !s.key.is_empty() {
            return Err(ApiError::invalid("the global scope has an empty key"));
        }
        if s.kind != ScopeKind::Global && s.key.is_empty() {
            return Err(ApiError::invalid(format!("scope {} needs a key", s.kind)));
        }
        if !out.contains(s) {
            out.push(s.clone());
        }
    }
    Ok(out)
}

/// Global + the caller's current project (`clients.current_project_key`) + the caller's device.
pub async fn default_scopes(conn: &mut PgConnection, identity: &Identity) -> Result<Vec<Scope>, sqlx::Error> {
    let mut scopes = vec![Scope::global()];
    let project: Option<String> = sqlx::query_scalar("SELECT current_project_key FROM clients WHERE id = $1")
        .bind(identity.client_id)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        scopes.push(Scope::new(ScopeKind::Project, p));
    }
    if !identity.device_id.is_empty() {
        scopes.push(Scope::new(ScopeKind::Device, identity.device_id.clone()));
    }
    Ok(scopes)
}

// ---------- the pack ----------

#[derive(Debug, Clone)]
pub struct PackOptions {
    pub query: Option<String>,
    pub entities: Vec<String>,
    pub intent: Option<String>,
    pub budgets: Budgets,
    pub expand: u32,
    pub rerank: bool,
    pub exclude_sensitivity: Vec<Sensitivity>,
    /// SessionStart: no query, pinned facts only, subagent observations hidden, session summaries
    /// only (at most [`CONTEXT_SUMMARIES`]), at most [`CONTEXT_OBS_ROWS`] index rows.
    pub context: bool,
}

pub const CONTEXT_OBS_ROWS: usize = 30;
pub const CONTEXT_SUMMARIES: usize = 5;

/// Slots plus what `retrieval_log` records.
#[derive(Debug, Clone)]
pub struct Pack {
    pub slots: RecallSlots,
    pub by_slot: BTreeMap<String, i32>,
    pub candidates: Vec<Candidate>,
    pub selected: Vec<Value>,
    pub reranked: bool,
}

fn rerank_text(row: &TierRow) -> String {
    let join = |parts: &[Option<&str>]| parts.iter().flatten().copied().collect::<Vec<_>>().join(" ");
    match row {
        TierRow::Obs(o) => join(&[Some(&o.title), o.subtitle.as_deref(), o.narrative.as_deref()]),
        TierRow::Sum(s) => {
            join(&[s.request.as_deref(), s.learned.as_deref(), s.completed.as_deref(), s.next_steps.as_deref()])
        }
        TierRow::Fact(f) => format!("{} {} {}", f.subject_name, f.predicate, f.value_text),
        TierRow::Proc(p) => format!("{} {}", p.name, p.trigger_text),
    }
}

async fn embed_query(state: &AppState, query: &str) -> Option<Vec<f32>> {
    match state.tei.embed(&[query.to_string()]).await {
        Ok(mut v) if !v.is_empty() => Some(v.swap_remove(0)),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "query embedding failed, keyword leg only");
            None
        }
    }
}

/// Rerank the fused top 40 across tiers; the rerank score replaces RRF as the base (11.3).
/// False on timeout or error, leaving RRF in place.
async fn rerank(state: &AppState, query: &str, cands: &mut [&mut Candidate]) -> bool {
    cands.sort_by(|a, b| b.rrf.total_cmp(&a.rrf).then(a.table.cmp(&b.table)).then(a.id.cmp(&b.id)));
    let n = cands.len().min(RERANK_TOP);
    let top = &mut cands[..n];
    if top.is_empty() {
        return false;
    }
    let texts: Vec<String> = top.iter().map(|c| c.row.as_ref().map(rerank_text).unwrap_or_default()).collect();
    let timeout = state.settings.tei_rerank_timeout;
    match tokio::time::timeout(timeout, state.tei.rerank(query, &texts)).await {
        Ok(Ok(scores)) if scores.len() == top.len() => {
            for (c, s) in top.iter_mut().zip(scores) {
                c.rerank = Some(f64::from(s));
            }
            true
        }
        Ok(Ok(_)) => {
            tracing::warn!("rerank returned a different number of scores; using RRF");
            false
        }
        Ok(Err(e)) => {
            tracing::info!(error = %e, "rerank failed; using RRF");
            false
        }
        Err(_) => {
            tracing::info!(?timeout, "rerank timed out; using RRF");
            false
        }
    }
}

fn budget_of(b: &Budgets, table: MemoryTable) -> u32 {
    match table {
        MemoryTable::Facts => b.facts,
        MemoryTable::Procedures => b.procedures,
        MemoryTable::Observations => b.observations,
        MemoryTable::Summaries => b.summaries,
    }
}

/// Candidates, ranking and budget fill for one recall or SessionStart render (spec 11.2-11.5).
pub async fn build_pack(
    state: &AppState,
    conn: &mut PgConnection,
    spaces: &[Uuid],
    scopes: &[Scope],
    opts: &PackOptions,
) -> Result<Pack, ApiError> {
    let now = Utc::now();
    let budgets = opts.budgets;
    let query = opts.query.as_deref().map(str::trim).filter(|q| !q.is_empty());
    let qvec = match query {
        Some(q) => embed_query(state, q).await,
        None => None,
    };
    let db_caps = caps(&state.db.app).await?;
    if qvec.is_some() {
        tune_vector_search(conn, &db_caps, ef_search_for(VECTOR_LIMIT)).await?;
    }
    let terms = query.map(query_terms).unwrap_or_default();
    let entity_terms = query_terms(&opts.entities.join(" "));
    let model = state.tei.model().to_string();

    let filters = Filters {
        spaces: spaces.to_vec(),
        scopes: Some(scopes.to_vec()),
        exclude_sensitivity: opts.exclude_sensitivity.clone(),
        summary_grains: Some(if opts.context { vec![SummaryGrain::Session] } else { RECALL_SUMMARY_GRAINS.to_vec() }),
        exclude_subagent: opts.context,
        ..Filters::default()
    };

    let mut by_tier: BTreeMap<MemoryTable, Vec<Candidate>> = BTreeMap::new();
    for table in TIER_ORDER {
        let mut cands = Vec::new();
        if budget_of(&budgets, table) > 0 && !(table == MemoryTable::Facts && opts.context) {
            if query.is_some() {
                let kw = if terms.is_empty() && entity_terms.is_empty() {
                    Vec::new()
                } else {
                    keyword_leg(conn, db_caps.pg_search, table, &filters, &terms, &entity_terms, KEYWORD_LIMIT).await?
                };
                let vec = match &qvec {
                    Some(v) => vector_leg(conn, table, &filters, v, &model, true, VECTOR_LIMIT).await?,
                    None => Vec::new(),
                };
                cands = rrf_fuse(table, &kw, &vec, FUSED_LIMIT);
            } else {
                let limit = if table == MemoryTable::Observations { NO_QUERY_OBS_LIMIT } else { NO_QUERY_LIMIT };
                cands = recent_leg(conn, table, &filters, limit, 0)
                    .await?
                    .into_iter()
                    .map(|id| Candidate { rrf: 1.0, ..Candidate::new(id, table) })
                    .collect();
            }
        }
        by_tier.insert(table, cands);
    }

    // Pinned facts are added regardless of score (11.2).
    let mut pinned_ids: Vec<Uuid> = Vec::new();
    if budgets.facts > 0 {
        let f = Filters { pinned_only: true, ..filters.clone() };
        pinned_ids = recent_leg(conn, MemoryTable::Facts, &f, PINNED_LIMIT, 0).await?;
        let facts = by_tier.entry(MemoryTable::Facts).or_default();
        for id in &pinned_ids {
            if !facts.iter().any(|c| c.id == *id) {
                facts.push(Candidate::new(*id, MemoryTable::Facts));
            }
        }
    }

    let (profile, profile_fact_ids, profile_cost) = profile_slot(conn, spaces, scopes, opts).await?;

    for (table, cands) in by_tier.iter_mut() {
        let ids: Vec<Uuid> = cands.iter().map(|c| c.id).collect();
        let mut rows = hydrate(conn, *table, &ids).await?;
        cands.retain_mut(|c| {
            c.row = rows.remove(&c.id);
            c.row.is_some()
        });
    }

    let mut reranked = false;
    if let (true, Some(q)) = (opts.rerank, query) {
        let mut fused: Vec<&mut Candidate> = by_tier.values_mut().flatten().filter(|c| c.rrf > 0.0).collect();
        reranked = rerank(state, q, &mut fused).await;
    }
    if reranked {
        // Rerank scores and RRF are different scales: rows past the reranked top 40 cannot be
        // ranked against them, so they drop out (pinned facts are placed first regardless).
        for (table, cands) in by_tier.iter_mut() {
            cands.retain(|c| {
                c.rerank.is_some() || c.rrf <= 0.0 || (*table == MemoryTable::Facts && pinned_ids.contains(&c.id))
            });
        }
    }
    for c in by_tier.values_mut().flatten() {
        c.score = c.compute_score(now, opts.intent.as_deref());
    }
    let keys: Vec<(MemoryTable, Uuid)> = by_tier.values().flatten().map(|c| (c.table, c.id)).collect();
    let mut embeddings = best_part_embeddings(conn, &keys, &model, qvec.as_deref()).await?;
    for c in by_tier.values_mut().flatten() {
        c.embedding = embeddings.remove(&(c.table, c.id));
    }
    let candidates: Vec<Candidate> = by_tier.values().flatten().cloned().collect();
    let mut take = |t: MemoryTable| by_tier.remove(&t).unwrap_or_default();

    let mut slots = RecallSlots { profile, ..RecallSlots::default() };
    let mut by_slot = BTreeMap::from([("profile".to_string(), profile_cost)]);

    // Facts: pinned first, then MMR order; facts already shown in a fallback profile are skipped.
    let pinned_pos: HashMap<Uuid, usize> = pinned_ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let pinned_set: HashSet<Uuid> = pinned_ids.into_iter().collect();
    let (mut pinned, rest): (Vec<Candidate>, Vec<Candidate>) = take(MemoryTable::Facts)
        .into_iter()
        .filter(|c| !profile_fact_ids.contains(&c.id))
        .partition(|c| pinned_set.contains(&c.id));
    // Pinned facts no leg returned score 0: they keep the order of the pinned query (importance x confidence).
    pinned.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| pinned_pos.get(&a.id).cmp(&pinned_pos.get(&b.id))));
    let ranked = if opts.context { Vec::new() } else { mmr_order(rest.into_iter().filter(|c| c.rrf > 0.0).collect()) };
    let fact_items: Vec<FactItem> = pinned.iter().chain(&ranked).filter_map(fact_item).collect();
    let (facts, used) = budget_fill(fact_items, budgets.facts, |f| estimate_tokens(&render::fact_line(f)));
    slots.facts = facts;
    by_slot.insert("facts".into(), used);

    let proc_items: Vec<ProcedureItem> =
        mmr_order(take(MemoryTable::Procedures)).iter().filter_map(procedure_item).collect();
    let (procedures, used) =
        budget_fill(proc_items, budgets.procedures, |p| estimate_tokens(&render::procedure_line(p)));
    slots.procedures = procedures;
    by_slot.insert("procedures".into(), used);

    let max_rows = opts.context.then_some(CONTEXT_OBS_ROWS);
    let (mut observations, used) =
        fill_observations(mmr_order(take(MemoryTable::Observations)), budgets.observations, opts.expand, max_rows);
    if opts.context {
        // The SessionStart index reads newest first, like claude-mem's.
        observations.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.item.id.cmp(&b.item.id)));
    }
    slots.observations = observations;
    by_slot.insert("observations".into(), used);

    let mut sum_items: Vec<SummaryItem> =
        mmr_order(take(MemoryTable::Summaries)).iter().filter_map(summary_item).collect();
    if opts.context {
        sum_items.truncate(CONTEXT_SUMMARIES);
    }
    let (summaries, used) = budget_fill(sum_items, budgets.summaries, |s| estimate_tokens(&render::summary_line(s)));
    slots.summaries = summaries;
    by_slot.insert("summaries".into(), used);

    if budgets.work_state > 0 {
        let lists = fold_work_state(conn, spaces, scopes).await?;
        let (lists, used) = budget_fill(lists, budgets.work_state, |w| w.token_estimate);
        slots.work_state = lists;
        by_slot.insert("work_state".into(), used);
    } else {
        by_slot.insert("work_state".into(), 0);
    }

    let entry = |slot: &str, item: &SlotItem| json!({ "id": item.id, "slot": slot, "citation": item.citation });
    let mut selected: Vec<Value> = Vec::new();
    selected.extend(slots.facts.iter().map(|i| entry("facts", &i.item)));
    selected.extend(slots.procedures.iter().map(|i| entry("procedures", &i.item)));
    selected.extend(slots.observations.iter().map(|i| entry("observations", &i.item)));
    selected.extend(slots.summaries.iter().map(|i| entry("summaries", &i.item)));
    selected.extend(profile_fact_ids.iter().map(|id| json!({ "id": id, "slot": "profile" })));

    Ok(Pack { slots, by_slot, candidates, selected, reranked })
}

fn fill_observations(
    ordered: Vec<Candidate>,
    budget: u32,
    expand: u32,
    max_rows: Option<usize>,
) -> (Vec<ObservationItem>, i32) {
    let budget = i64::from(budget);
    let mut out: Vec<ObservationItem> = Vec::new();
    let (mut used, mut expanded) = (0i64, 0u32);
    for c in &ordered {
        if max_rows.is_some_and(|m| out.len() >= m) {
            break;
        }
        if expanded < expand
            && let Some(item) = observation_item(c, true)
        {
            let cost = i64::from(if item.item.token_estimate > 0 {
                item.item.token_estimate
            } else {
                estimate_tokens(&render::observation_block(&item))
            });
            if used + cost <= budget {
                used += cost;
                expanded += 1;
                out.push(item);
                continue;
            }
        }
        if let Some(item) = observation_item(c, false) {
            let cost =
                i64::from(estimate_tokens(&render::index_line(&render::IndexEntry::from_observation(&item), false)));
            if used + cost <= budget {
                used += cost;
                out.push(item);
            }
        }
    }
    (out, i32::try_from(used).unwrap_or(i32::MAX))
}

/// Profile slot (architecture 6.1 step 1): rendered `profiles` rows for the scopes (scope order,
/// then space order) within the budget; without any, a fallback rendering of the top active facts,
/// whose ids are returned so the facts slot does not repeat them.
async fn profile_slot(
    conn: &mut PgConnection,
    spaces: &[Uuid],
    scopes: &[Scope],
    opts: &PackOptions,
) -> Result<(Option<ProfileSlot>, Vec<Uuid>, i32), ApiError> {
    let budget = i64::from(opts.budgets.profile);
    if budget <= 0 {
        return Ok((None, Vec::new(), 0));
    }
    let kinds: Vec<ScopeKind> = scopes.iter().map(|s| s.kind).collect();
    let keys: Vec<String> = scopes.iter().map(|s| s.key.clone()).collect();
    let mut rows: Vec<(Uuid, ScopeKind, String, String)> = sqlx::query_as(
        "SELECT space_id, scope_kind, scope_key, body_md FROM profiles
          WHERE space_id = ANY($1)
            AND (scope_kind, scope_key) IN (SELECT * FROM unnest($2::scope_kind[], $3::text[]))",
    )
    .bind(spaces.to_vec())
    .bind(kinds)
    .bind(keys)
    .fetch_all(&mut *conn)
    .await?;
    if !rows.is_empty() {
        let pos = |r: &(Uuid, ScopeKind, String, String)| {
            let scope = Scope::new(r.1, r.2.clone());
            (
                scopes.iter().position(|s| *s == scope).unwrap_or(usize::MAX),
                spaces.iter().position(|s| *s == r.0).unwrap_or(usize::MAX),
            )
        };
        rows.sort_by_key(|r| pos(r));
        let mut lines: Vec<&str> = Vec::new();
        let mut used = 0i64;
        let mut used_scopes: Vec<Scope> = Vec::new();
        for r in &rows {
            for line in r.3.trim().lines() {
                let cost = i64::from(estimate_tokens(line)) + 1;
                if used + cost > budget {
                    break;
                }
                lines.push(line);
                used += cost;
                let scope = Scope::new(r.1, r.2.clone());
                if !used_scopes.contains(&scope) {
                    used_scopes.push(scope);
                }
            }
        }
        if lines.is_empty() {
            return Ok((None, Vec::new(), 0));
        }
        let body = lines.join("\n");
        let tokens = estimate_tokens(&body);
        return Ok((
            Some(ProfileSlot { body_md: body, token_estimate: tokens, scopes: used_scopes }),
            Vec::new(),
            tokens,
        ));
    }
    // Fallback: no profile rendered yet for these scopes -> top active facts.
    let f = Filters {
        spaces: spaces.to_vec(),
        scopes: Some(scopes.to_vec()),
        exclude_sensitivity: opts.exclude_sensitivity.clone(),
        ..Filters::default()
    };
    let ids = recent_leg(conn, MemoryTable::Facts, &f, PINNED_LIMIT, 0).await?;
    let mut rows = hydrate(conn, MemoryTable::Facts, &ids).await?;
    let items: Vec<FactItem> = ids
        .iter()
        .filter_map(|id| rows.remove(id))
        .filter_map(|row| fact_item(&Candidate { row: Some(row), ..Candidate::new(Uuid::nil(), MemoryTable::Facts) }))
        .collect();
    let (chosen, _) = budget_fill(items, opts.budgets.profile, |f| estimate_tokens(&render::fact_line(f)) + 1);
    if chosen.is_empty() {
        return Ok((None, Vec::new(), 0));
    }
    let body = chosen.iter().map(render::fact_line).collect::<Vec<_>>().join("\n");
    let mut used_scopes: Vec<Scope> = Vec::new();
    for f in &chosen {
        if !used_scopes.contains(&f.item.scope) {
            used_scopes.push(f.item.scope.clone());
        }
    }
    let tokens = estimate_tokens(&body);
    let ids = chosen.iter().map(|f| f.item.id).collect();
    Ok((Some(ProfileSlot { body_md: body, token_estimate: tokens, scopes: used_scopes }), ids, tokens))
}

/// Folded work-state lists (last write per key wins; a null value removes the key), newest list
/// first (spec 6.5).
pub async fn fold_work_state(
    conn: &mut PgConnection,
    spaces: &[Uuid],
    scopes: &[Scope],
) -> Result<Vec<WorkStateList>, sqlx::Error> {
    let kinds: Vec<ScopeKind> = scopes.iter().map(|s| s.kind).collect();
    let keys: Vec<String> = scopes.iter().map(|s| s.key.clone()).collect();
    let rows: Vec<(ScopeKind, String, String, Value, DateTime<Utc>)> = sqlx::query_as(
        "SELECT scope_kind, scope_key, list_name, fields, created_at FROM work_state
          WHERE space_id = ANY($1)
            AND (scope_kind, scope_key) IN (SELECT * FROM unnest($2::scope_kind[], $3::text[]))
          ORDER BY created_at, id",
    )
    .bind(spaces.to_vec())
    .bind(kinds)
    .bind(keys)
    .fetch_all(conn)
    .await?;
    let mut folded: Vec<WorkStateList> = Vec::new();
    for (kind, key, list_name, fields, created_at) in rows {
        let scope = Scope::new(kind, key);
        let i = match folded.iter().position(|w| w.scope == scope && w.list_name == list_name) {
            Some(i) => i,
            None => {
                folded.push(WorkStateList {
                    scope,
                    list_name,
                    fields: serde_json::Map::new(),
                    updated_at: None,
                    token_estimate: 0,
                });
                folded.len() - 1
            }
        };
        let w = &mut folded[i];
        if let Value::Object(map) = fields {
            for (k, v) in map {
                if v.is_null() {
                    w.fields.remove(&k);
                } else {
                    w.fields.insert(k, v);
                }
            }
        }
        w.updated_at = Some(created_at);
    }
    folded.retain(|w| !w.fields.is_empty());
    for w in &mut folded {
        w.token_estimate = estimate_tokens(&render::work_state_line(w));
    }
    folded.sort_by(|a, b| {
        b.updated_at.cmp(&a.updated_at).then(a.list_name.cmp(&b.list_name)).then(a.scope.cmp(&b.scope))
    });
    Ok(folded)
}

// ---------- slot items ----------

fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

fn slot_item(c: &Candidate, row: &TierRow, token_estimate: i32, citation: String) -> SlotItem {
    SlotItem {
        id: row.id(),
        space_id: row.space_id(),
        scope: row.scope(),
        score: round6(c.score),
        token_estimate,
        citation: Some(citation),
    }
}

fn fact_item(c: &Candidate) -> Option<FactItem> {
    let row @ TierRow::Fact(r) = c.row.as_ref()? else { return None };
    let confidence = f64::from(r.confidence);
    let citation = render::fact_tag(&r.id, confidence, &r.devices, Some(r.first_seen_at));
    Some(FactItem {
        item: slot_item(c, row, estimate_tokens(&r.value_text), citation),
        subject: r.subject_name.clone(),
        subject_kind: Some(r.subject_kind.clone()),
        predicate: r.predicate.clone(),
        value: r.value.clone(),
        value_text: r.value_text.clone(),
        confidence,
        importance: f64::from(r.importance),
        pinned: r.pinned,
        learned_on: r.devices.clone(),
        first_seen_at: Some(r.first_seen_at),
        last_confirmed_at: Some(r.last_confirmed_at),
    })
}

fn procedure_item(c: &Candidate) -> Option<ProcedureItem> {
    let row @ TierRow::Proc(r) = c.row.as_ref()? else { return None };
    let tokens = if r.token_estimate > 0 { r.token_estimate } else { estimate_tokens(&r.body_md) };
    Some(ProcedureItem {
        item: slot_item(c, row, tokens, render::procedure_tag(&r.id, r.version, Some(r.created_at))),
        name: r.name.clone(),
        task_class: r.task_class.clone(),
        trigger_text: r.trigger_text.clone(),
        body_md: r.body_md.clone(),
        version: r.version,
        success_count: r.success_count,
        last_used_at: r.last_used_at,
    })
}

fn observation_item(c: &Candidate, expand: bool) -> Option<ObservationItem> {
    let row @ TierRow::Obs(r) = c.row.as_ref()? else { return None };
    let tokens = if r.token_estimate > 0 {
        r.token_estimate
    } else {
        let mut text =
            vec![r.title.as_str(), r.subtitle.as_deref().unwrap_or(""), r.narrative.as_deref().unwrap_or("")];
        text.extend(r.facts.iter().map(String::as_str));
        estimate_tokens(&text.join(" "))
    };
    Some(ObservationItem {
        item: slot_item(c, row, tokens, render::obs_tag(&r.id, r.device_id.as_deref(), Some(r.created_at))),
        obs_type: r.obs_type.clone(),
        title: r.title.clone(),
        subtitle: r.subtitle.clone(),
        narrative: if expand { r.narrative.clone() } else { None },
        facts: if expand { r.facts.clone() } else { Vec::new() },
        files: r.files.clone(),
        importance: f64::from(r.importance),
        created_at: r.created_at,
        session_id: r.session_id,
        device_id: r.device_id.clone(),
        expanded: expand,
    })
}

fn summary_item(c: &Candidate) -> Option<SummaryItem> {
    let row @ TierRow::Sum(r) = c.row.as_ref()? else { return None };
    Some(SummaryItem {
        item: slot_item(
            c,
            row,
            r.token_estimate,
            render::summary_tag(&r.id, r.device_id.as_deref(), Some(r.created_at)),
        ),
        grain: r.grain,
        request: r.request.clone(),
        learned: r.learned.clone(),
        completed: r.completed.clone(),
        next_steps: r.next_steps.clone(),
        created_at: r.created_at,
        session_id: r.session_id,
        device_id: r.device_id.clone(),
    })
}

// ---------- POST /v1/recall ----------

/// What a `retrieval_log` row records.
pub struct RetrievalRecord<'a> {
    pub mode: &'a str,
    pub query: Option<&'a str>,
    pub spaces: &'a [Uuid],
    pub scopes: &'a [Scope],
    pub budgets: Option<&'a Budgets>,
    pub latency_ms: i64,
}

/// Insert a `retrieval_log` row (in the caller's `user_tx`) and return its id.
pub async fn log_retrieval(
    conn: &mut PgConnection,
    identity: &Identity,
    rec: RetrievalRecord<'_>,
    pack: &Pack,
) -> Result<Uuid, sqlx::Error> {
    let id = new_id();
    let mut cands: Vec<&Candidate> = pack.candidates.iter().collect();
    cands.sort_by(|a, b| a.table.cmp(&b.table).then(by_score(a, b)));
    let logged: Vec<Value> = cands.iter().take(MAX_LOGGED_CANDIDATES).map(|c| c.log()).collect();
    let scopes: Vec<String> = rec.scopes.iter().map(Scope::as_text).collect();
    sqlx::query(
        "INSERT INTO retrieval_log (id, space_id, client_id, mode, query, scopes, budgets, candidates, selected,
                                    latency_ms, reranked)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(rec.spaces[0])
    .bind(identity.client_id)
    .bind(rec.mode)
    .bind(rec.query)
    .bind(json!({ "space_ids": rec.spaces, "scopes": scopes }))
    .bind(rec.budgets.map(|b| json!(b)))
    .bind(Value::Array(logged))
    .bind(Value::Array(pack.selected.clone()))
    .bind(i32::try_from(rec.latency_ms).unwrap_or(i32::MAX))
    .bind(pack.reranked)
    .execute(conn)
    .await?;
    Ok(id)
}

/// Prometheus counters for one served pack.
pub fn record_metrics(mode: &'static str, pack: &Pack, elapsed_s: f64) {
    let reranked = if pack.reranked { "true" } else { "false" };
    metrics::histogram!("memsvc_recall_latency_seconds", "mode" => mode, "reranked" => reranked).record(elapsed_s);
    metrics::histogram!("memsvc_recall_candidates", "mode" => mode).record(pack.candidates.len() as f64);
    metrics::histogram!("memsvc_recall_selected", "mode" => mode).record(pack.selected.len() as f64);
    for (slot, tokens) in &pack.by_slot {
        metrics::counter!("memsvc_tokens_injected_total", "mode" => mode, "slot" => slot.clone())
            .increment(u64::try_from(*tokens).unwrap_or(0));
    }
}

/// Push-mode recall: per-tier BM25 (or tsvector fallback) + vector candidates, RRF, optional
/// rerank, final score, MMR, budget fill, markdown render; writes `retrieval_log` (spec 11.1-11.5).
pub async fn recall(state: &AppState, identity: &Identity, req: RecallRequest) -> Result<RecallResponse, ApiError> {
    req.validate()?;
    let started = Instant::now();
    let mut tx = user_tx(&state.db.app, identity).await?;
    let spaces = resolve_read_spaces(&mut tx, identity, req.space_ids.as_deref()).await?;
    if spaces.is_empty() {
        return Err(ApiError::forbidden("space_forbidden", "the caller has no readable space"));
    }
    let scopes = match req.scopes.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => validate_scopes(s)?,
        None => default_scopes(&mut tx, identity).await?,
    };
    let task_state = req.task_state.clone().unwrap_or_default();
    let opts = PackOptions {
        query: req.query.clone(),
        entities: task_state.entities,
        intent: task_state.intent,
        budgets: req.budgets,
        expand: req.expand_top_observations,
        rerank: req.rerank,
        exclude_sensitivity: req.exclude_sensitivity.clone(),
        context: false,
    };
    let pack = build_pack(state, &mut tx, &spaces, &scopes, &opts).await?;
    let rendered = (req.format != RecallFormat::Json).then(|| render::recall_markdown(&pack.slots));
    let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let rec = RetrievalRecord {
        mode: "recall",
        query: req.query.as_deref(),
        spaces: &spaces,
        scopes: &scopes,
        budgets: Some(&req.budgets),
        latency_ms,
    };
    let retrieval_id = log_retrieval(&mut tx, identity, rec, &pack).await?;
    log_pack_access(&mut tx, identity, "recall", &pack.slots).await?;
    tx.commit().await?;
    record_metrics("recall", &pack, started.elapsed().as_secs_f64());
    Ok(RecallResponse {
        retrieval_id,
        tokens: TokenUsage { total: pack.by_slot.values().sum(), by_slot: pack.by_slot.clone() },
        slots: pack.slots,
        rendered_markdown: rendered,
        latency_ms,
        reranked: pack.reranked,
    })
}

/// `POST /v1/recall/{retrieval_id}/feedback`: stores the feedback on the retrieval and bumps
/// `access_count`/`last_accessed_at` of used facts (`last_used_at` of used procedures) among the
/// retrieval's selected ids. Only the client that made the retrieval may report on it.
pub async fn feedback(
    state: &AppState,
    identity: &Identity,
    retrieval_id: Uuid,
    req: FeedbackRequest,
) -> Result<FeedbackResponse, ApiError> {
    let mut tx = user_tx(&state.db.app, identity).await?;
    let fb = json!({ "used_ids": req.used_ids, "helpful": req.helpful, "notes": req.notes, "at": Utc::now() });
    let found: Option<Uuid> =
        sqlx::query_scalar("UPDATE retrieval_log SET feedback = $3 WHERE id = $1 AND client_id = $2 RETURNING id")
            .bind(retrieval_id)
            .bind(identity.client_id)
            .bind(fb)
            .fetch_optional(&mut *tx)
            .await?;
    if found.is_none() {
        return Err(ApiError::not_found("retrieval not found"));
    }
    if !req.used_ids.is_empty() {
        let used = "id = ANY($1) AND id::text IN (SELECT jsonb_array_elements(selected)->>'id' FROM retrieval_log WHERE id = $2)";
        sqlx::query(&format!(
            "UPDATE facts SET access_count = access_count + 1, last_accessed_at = now() WHERE {used}"
        ))
        .bind(&req.used_ids)
        .bind(retrieval_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!("UPDATE procedures SET last_used_at = now() WHERE {used}"))
            .bind(&req.used_ids)
            .bind(retrieval_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    metrics::counter!("memsvc_feedback_used_total").increment(req.used_ids.len() as u64);
    Ok(FeedbackResponse { retrieval_id, recorded: true })
}

// ---------- index rows (search, timeline, by-file) ----------

fn index_row(row: &TierRow, score: Option<f64>, anchor: bool) -> IndexRow {
    let (table, row_type, title, created_at, tokens) = match row {
        TierRow::Obs(r) => {
            let tokens = if r.token_estimate > 0 {
                r.token_estimate
            } else {
                estimate_tokens(&format!("{} {}", r.title, r.narrative.as_deref().unwrap_or("")))
            };
            (MemoryTable::Observations, r.obs_type.clone(), r.title.clone(), r.created_at, tokens)
        }
        TierRow::Sum(r) => {
            let title = [&r.request, &r.learned, &r.completed, &r.next_steps]
                .into_iter()
                .flatten()
                .find(|t| !t.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| format!("{} summary", r.grain));
            let tokens = if r.token_estimate > 0 {
                r.token_estimate
            } else {
                let parts: Vec<&str> = [&r.request, &r.learned, &r.completed, &r.next_steps]
                    .into_iter()
                    .flatten()
                    .map(String::as_str)
                    .collect();
                estimate_tokens(&parts.join(" "))
            };
            (MemoryTable::Summaries, r.grain.to_string(), title, r.created_at, tokens)
        }
        TierRow::Fact(r) => (
            MemoryTable::Facts,
            r.predicate.clone(),
            format!("{}: {}", r.predicate, r.value_text),
            r.first_seen_at,
            estimate_tokens(&r.value_text),
        ),
        TierRow::Proc(r) => {
            let tokens = if r.token_estimate > 0 { r.token_estimate } else { estimate_tokens(&r.body_md) };
            (MemoryTable::Procedures, r.task_class.clone(), r.name.clone(), r.created_at, tokens)
        }
    };
    IndexRow {
        id: row.id(),
        table,
        row_type: Some(row_type),
        title: render::one_line(&title).chars().take(200).collect(),
        created_at,
        scope: row.scope(),
        token_estimate: tokens,
        score: score.map(round6),
        is_anchor: anchor,
    }
}

/// Index rows for `GET /v1/search` and `memory_search` (spec 6.4, 7). Secret rows are never listed.
pub async fn search(state: &AppState, identity: &Identity, query: SearchQuery) -> Result<SearchResponse, ApiError> {
    query.validate()?;
    let mut tx = user_tx(&state.db.app, identity).await?;
    let spaces = resolve_read_spaces(&mut tx, identity, query.space_ids.as_deref()).await?;
    let scopes = query.scopes.as_deref().map(validate_scopes).transpose()?;
    let mut tables = query.tables.clone().unwrap_or_else(|| vec![MemoryTable::Observations, MemoryTable::Summaries]);
    if query.files.is_some() {
        tables.retain(|t| *t == MemoryTable::Observations);
    }
    let empty = SearchResponse { rows: Vec::new(), limit: query.limit, offset: query.offset };
    if spaces.is_empty() || tables.is_empty() {
        return Ok(empty);
    }
    let want = query.offset.saturating_add(query.limit);
    let f = Filters {
        spaces,
        scopes,
        exclude_sensitivity: vec![Sensitivity::Secret],
        types: query.types.clone(),
        since: query.since,
        until: query.until,
        files: query.files.clone(),
        ..Filters::default()
    };
    let now = Utc::now();
    // (sort key, row): score descending or newest first, then id.
    let mut rows: Vec<((f64, Uuid), IndexRow)> = Vec::new();
    match query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => {
            let terms = query_terms(q);
            let qvec = embed_query(state, q).await;
            let db_caps = caps(&state.db.app).await?;
            if qvec.is_some() {
                tune_vector_search(&mut tx, &db_caps, ef_search_for(VECTOR_LIMIT.max(want))).await?;
            }
            let model = state.tei.model().to_string();
            for table in tables {
                let kw = if terms.is_empty() {
                    Vec::new()
                } else {
                    keyword_leg(&mut tx, db_caps.pg_search, table, &f, &terms, &[], KEYWORD_LIMIT.max(want)).await?
                };
                let vec = match &qvec {
                    Some(v) => vector_leg(&mut tx, table, &f, v, &model, false, VECTOR_LIMIT.max(want)).await?,
                    None => Vec::new(),
                };
                let mut cands = rrf_fuse(table, &kw, &vec, usize::MAX);
                let ids: Vec<Uuid> = cands.iter().map(|c| c.id).collect();
                let mut hyd = hydrate(&mut tx, table, &ids).await?;
                for c in &mut cands {
                    c.row = hyd.remove(&c.id);
                    if let Some(row) = &c.row {
                        c.score = c.compute_score(now, None);
                        rows.push(((-c.score, c.id), index_row(row, Some(c.score), false)));
                    }
                }
            }
        }
        None => {
            for table in tables {
                let ids = recent_leg(&mut tx, table, &f, want, 0).await?;
                let mut hyd = hydrate(&mut tx, table, &ids).await?;
                for id in ids {
                    if let Some(row) = hyd.remove(&id) {
                        let r = index_row(&row, None, false);
                        rows.push(((-(r.created_at.timestamp_millis() as f64), id), r));
                    }
                }
            }
        }
    }
    tx.commit().await?;
    rows.sort_by(|a, b| a.0.0.total_cmp(&b.0.0).then(a.0.1.cmp(&b.0.1)));
    let page = rows.into_iter().skip(query.offset as usize).take(query.limit as usize).map(|(_, r)| r).collect();
    Ok(SearchResponse { rows: page, limit: query.limit, offset: query.offset })
}

/// Observations and summaries around an anchor in time order, same space and scope (spec 6.4).
pub async fn timeline(
    state: &AppState,
    identity: &Identity,
    query: TimelineQuery,
) -> Result<TimelineResponse, ApiError> {
    query.validate()?;
    let mut tx = user_tx(&state.db.app, identity).await?;
    let anchor: Option<(Uuid, ScopeKind, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT space_id, scope_kind, scope_key, created_at FROM observations WHERE id = $1
         UNION ALL
         SELECT space_id, scope_kind, scope_key, created_at FROM summaries WHERE id = $1",
    )
    .bind(query.anchor_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((space, kind, key, at)) = anchor else {
        return Err(ApiError::not_found("anchor not found"));
    };
    let refs: Vec<(MemoryTable, Uuid)> = sqlx::query_as(
        "WITH items AS (
           SELECT 'observations' AS tbl, t.id, t.created_at FROM observations t
            WHERE t.space_id = $1 AND t.scope_kind = $2 AND t.scope_key = $3
              AND ((t.archived_at IS NULL AND t.superseded_by IS NULL AND t.sensitivity <> 'secret') OR t.id = $5)
           UNION ALL
           SELECT 'summaries', t.id, t.created_at FROM summaries t
            WHERE t.space_id = $1 AND t.scope_kind = $2 AND t.scope_key = $3)
         SELECT tbl, id FROM (
           (SELECT * FROM items WHERE (created_at, id) < ($4, $5) ORDER BY created_at DESC, id DESC LIMIT $6)
           UNION ALL (SELECT * FROM items WHERE id = $5)
           UNION ALL
           (SELECT * FROM items WHERE (created_at, id) > ($4, $5) ORDER BY created_at, id LIMIT $7)) w
         ORDER BY created_at, id",
    )
    .bind(space)
    .bind(kind)
    .bind(key)
    .bind(at)
    .bind(query.anchor_id)
    .bind(query.before)
    .bind(query.after)
    .fetch_all(&mut *tx)
    .await?;
    let mut hydrated: HashMap<Uuid, TierRow> = HashMap::new();
    for table in [MemoryTable::Observations, MemoryTable::Summaries] {
        let ids: Vec<Uuid> = refs.iter().filter(|r| r.0 == table).map(|r| r.1).collect();
        hydrated.extend(hydrate(&mut tx, table, &ids).await?);
    }
    tx.commit().await?;
    let rows = refs
        .iter()
        .filter_map(|(_, id)| hydrated.get(id).map(|row| index_row(row, None, *id == query.anchor_id)))
        .collect();
    Ok(TimelineResponse { anchor_id: query.anchor_id, rows })
}

/// Recent observations touching a file (`GET /v1/observations/by-file`, spec 6.4). A stored path
/// matches when it equals the given path or one is a `/`-suffix of the other, so absolute and
/// repo-relative paths meet.
pub async fn by_file(state: &AppState, identity: &Identity, query: ByFileQuery) -> Result<SearchResponse, ApiError> {
    query.validate()?;
    let mut tx = user_tx(&state.db.app, identity).await?;
    let spaces = resolve_read_spaces(&mut tx, identity, None).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT t.id FROM observations t
          WHERE t.space_id = ANY($1) AND t.archived_at IS NULL AND t.superseded_by IS NULL
            AND t.sensitivity <> 'secret'
            AND EXISTS (SELECT 1 FROM unnest(t.files) AS fl(p)
                         WHERE fl.p = $2 OR right($2, length(fl.p) + 1) = '/' || fl.p
                            OR right(fl.p, length($2) + 1) = '/' || $2)
            AND ($4::text IS NULL
                 OR (t.scope_kind = 'project' AND t.scope_key = $4)
                 OR t.session_id IN (SELECT id FROM sessions WHERE project_key = $4))
          ORDER BY t.created_at DESC, t.id DESC LIMIT $3",
    )
    .bind(spaces)
    .bind(&query.path)
    .bind(query.limit)
    .bind(query.project_key.as_deref().filter(|p| !p.is_empty()))
    .fetch_all(&mut *tx)
    .await?;
    let mut hyd = hydrate(&mut tx, MemoryTable::Observations, &ids).await?;
    tx.commit().await?;
    let rows = ids.iter().filter_map(|id| hyd.remove(id)).map(|row| index_row(&row, None, false)).collect();
    Ok(SearchResponse { rows, limit: query.limit, offset: 0 })
}

// ---------- full bodies ----------

fn memory_item(row: TierRow) -> MemoryItem {
    match row {
        TierRow::Obs(r) => MemoryItem::Observations(ObservationFull {
            id: r.id,
            space_id: r.space_id,
            scope: Scope::new(r.scope_kind, r.scope_key),
            session_id: r.session_id,
            window_no: r.window_no,
            obs_type: r.obs_type,
            title: r.title,
            subtitle: r.subtitle,
            narrative: r.narrative,
            facts: r.facts,
            concepts: r.concepts,
            entities: r.entities,
            files: r.files,
            importance: f64::from(r.importance),
            sensitivity: r.sensitivity,
            source_event_ids: r.source_event_ids,
            extracted_by: r.extracted_by,
            written_by_client_id: r.written_by_client_id,
            token_estimate: r.token_estimate,
            created_at: r.created_at,
            archived_at: r.archived_at,
            superseded_by: r.superseded_by,
        }),
        TierRow::Sum(r) => MemoryItem::Summaries(SummaryFull {
            id: r.id,
            space_id: r.space_id,
            scope: Scope::new(r.scope_kind, r.scope_key),
            grain: r.grain,
            session_id: r.session_id,
            prompt_no: r.prompt_no,
            period_start: r.period_start,
            period_end: r.period_end,
            request: r.request,
            investigated: r.investigated,
            learned: r.learned,
            completed: r.completed,
            next_steps: r.next_steps,
            notes: r.notes,
            extracted_by: r.extracted_by,
            token_estimate: r.token_estimate,
            created_at: r.created_at,
        }),
        TierRow::Fact(r) => MemoryItem::Facts(FactFull {
            id: r.id,
            space_id: r.space_id,
            scope: Scope::new(r.scope_kind, r.scope_key),
            subject: EntityRef { id: r.subject_id, kind: r.subject_kind, canonical_name: r.subject_name },
            predicate: r.predicate,
            value: r.value,
            value_text: r.value_text,
            confidence: f64::from(r.confidence),
            importance: f64::from(r.importance),
            status: r.status,
            conflict: r.conflict,
            supersedes_id: r.supersedes_id,
            superseded_by: r.superseded_by,
            valid_from: r.valid_from,
            valid_to: r.valid_to,
            first_seen_at: r.first_seen_at,
            last_confirmed_at: r.last_confirmed_at,
            confirmations: r.confirmations,
            evidence_observation_ids: r.evidence_observation_ids,
            learned_on_client_ids: r.learned_on_client_ids,
            access_count: r.access_count,
            last_accessed_at: r.last_accessed_at,
            pinned: r.pinned,
            sensitivity: r.sensitivity,
            written_by_client_id: r.written_by_client_id,
            extracted_by: r.extracted_by,
        }),
        TierRow::Proc(r) => MemoryItem::Procedures(ProcedureFull {
            id: r.id,
            space_id: r.space_id,
            scope: Scope::new(r.scope_kind, r.scope_key),
            name: r.name,
            task_class: r.task_class,
            trigger_text: r.trigger_text,
            body_md: r.body_md,
            version: r.version,
            status: if r.status == "retired" { ProcedureStatus::Retired } else { ProcedureStatus::Active },
            supersedes_id: r.supersedes_id,
            success_count: r.success_count,
            failure_count: r.failure_count,
            last_used_at: r.last_used_at,
            source_observation_ids: r.source_observation_ids,
            token_estimate: r.token_estimate,
            created_at: r.created_at,
        }),
    }
}

/// Full bodies for ids across the four tiers (whatever RLS lets the caller see).
pub async fn load_items(conn: &mut PgConnection, ids: &[Uuid]) -> Result<HashMap<Uuid, MemoryItem>, sqlx::Error> {
    let mut out = HashMap::new();
    for table in TIER_ORDER {
        for (id, row) in hydrate(conn, table, ids).await? {
            out.insert(id, memory_item(row));
        }
    }
    Ok(out)
}

fn item_space(item: &MemoryItem) -> Uuid {
    match item {
        MemoryItem::Observations(o) => o.space_id,
        MemoryItem::Summaries(s) => s.space_id,
        MemoryItem::Facts(f) => f.space_id,
        MemoryItem::Procedures(p) => p.space_id,
    }
}

/// One `access_log` row per space touched.
pub async fn log_access(
    conn: &mut PgConnection,
    identity: &Identity,
    action: &str,
    items: &[&MemoryItem],
) -> Result<(), sqlx::Error> {
    let targets: Vec<(Uuid, Uuid)> = items.iter().map(|item| (item_space(item), item.id())).collect();
    log_access_ids(conn, identity, action, &targets).await
}

/// `access_log` for what a recall or context pack returned: fact values (also the ones rendered in
/// the profile, found again by their `fact#` tags), procedure bodies, summaries and expanded
/// observation narratives. Index-only observation lines (titles) are not content reads.
pub async fn log_pack_access(
    conn: &mut PgConnection,
    identity: &Identity,
    action: &str,
    slots: &RecallSlots,
) -> Result<(), sqlx::Error> {
    let mut targets: Vec<(Uuid, Uuid)> = Vec::new();
    targets.extend(slots.facts.iter().map(|f| (f.item.space_id, f.item.id)));
    targets.extend(slots.procedures.iter().map(|p| (p.item.space_id, p.item.id)));
    targets.extend(slots.summaries.iter().map(|s| (s.item.space_id, s.item.id)));
    targets.extend(slots.observations.iter().filter(|o| o.expanded).map(|o| (o.item.space_id, o.item.id)));
    if let Some(profile) = &slots.profile {
        let tags: Vec<String> = profile
            .body_md
            .split("fact#")
            .skip(1)
            .map(|rest| rest.chars().take(6).collect::<String>())
            .filter(|t| t.len() == 6 && t.chars().all(|c| c.is_ascii_hexdigit()))
            .collect();
        if !tags.is_empty() {
            let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT space_id, id FROM facts
                  WHERE status = 'active' AND right(replace(id::text, '-', ''), 6) = ANY($1)",
            )
            .bind(&tags)
            .fetch_all(&mut *conn)
            .await?;
            targets.extend(rows);
        }
    }
    log_access_ids(conn, identity, action, &targets).await
}

async fn log_access_ids(
    conn: &mut PgConnection,
    identity: &Identity,
    action: &str,
    targets: &[(Uuid, Uuid)],
) -> Result<(), sqlx::Error> {
    let mut per_space: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for (space, id) in targets {
        per_space.entry(*space).or_default().push(*id);
    }
    for (space, ids) in per_space {
        sqlx::query("INSERT INTO access_log (id, space_id, client_id, action, target_ids) VALUES ($1, $2, $3, $4, $5)")
            .bind(new_id())
            .bind(space)
            .bind(identity.client_id)
            .bind(action)
            .bind(ids)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Full bodies for up to 20 ids; writes `access_log` (spec 6.4).
pub async fn get_memories(state: &AppState, identity: &Identity, ids: &[Uuid]) -> Result<GetResponse, ApiError> {
    if ids.is_empty() || ids.len() > MAX_GET_IDS {
        return Err(ApiError::invalid(format!("ids must hold 1-{MAX_GET_IDS} items")));
    }
    let mut wanted: Vec<Uuid> = Vec::new();
    for id in ids {
        if !wanted.contains(id) {
            wanted.push(*id);
        }
    }
    let mut tx = user_tx(&state.db.app, identity).await?;
    let mut found = load_items(&mut tx, &wanted).await?;
    let items: Vec<MemoryItem> = wanted.iter().filter_map(|id| found.remove(id)).collect();
    let missing = wanted.iter().filter(|id| !items.iter().any(|i| i.id() == **id)).copied().collect();
    log_access(&mut tx, identity, "get", &items.iter().collect::<Vec<_>>()).await?;
    tx.commit().await?;
    Ok(GetResponse { items, missing })
}

/// Map display refs (`obs#a1b2c3`, the 6-hex short id, or a full id) to visible row ids, for the
/// MCP `memory_get` tool whose input may come from the SessionStart index. A ref matching no row,
/// or more than one, maps to `None`.
pub async fn resolve_refs(
    state: &AppState,
    identity: &Identity,
    refs: &[String],
) -> Result<Vec<(String, Option<Uuid>)>, ApiError> {
    let mut tx = user_tx(&state.db.app, identity).await?;
    let mut out = Vec::with_capacity(refs.len());
    for r in refs {
        let text = r.trim().to_lowercase();
        let text = text.rsplit('#').next().unwrap_or("").to_string();
        if let Ok(id) = text.parse::<Uuid>() {
            out.push((r.clone(), Some(id)));
            continue;
        }
        if text.len() < 4 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
            out.push((r.clone(), None));
            continue;
        }
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM observations WHERE replace(id::text, '-', '') LIKE '%' || $1
             UNION ALL SELECT id FROM summaries WHERE replace(id::text, '-', '') LIKE '%' || $1
             UNION ALL SELECT id FROM facts WHERE replace(id::text, '-', '') LIKE '%' || $1
             UNION ALL SELECT id FROM procedures WHERE replace(id::text, '-', '') LIKE '%' || $1
             LIMIT 2",
        )
        .bind(&text)
        .fetch_all(&mut *tx)
        .await?;
        out.push((r.clone(), if ids.len() == 1 { Some(ids[0]) } else { None }));
    }
    tx.commit().await?;
    Ok(out)
}

// ---------- explain (12.5) ----------

async fn provenance(
    conn: &mut PgConnection,
    session_id: Option<Uuid>,
    event_ids: &[Uuid],
    extracted_by: Value,
) -> Result<Provenance, sqlx::Error> {
    let mut session = None;
    if let Some(sid) = session_id {
        type Row = (Uuid, String, Option<String>, DateTime<Utc>, Option<String>, Option<String>, Option<String>);
        let row: Option<Row> = sqlx::query_as(
            "SELECT s.id, s.harness, s.project_key, s.started_at, c.device_id, c.hostname, c.kind
               FROM sessions s LEFT JOIN clients c ON c.id = s.client_id WHERE s.id = $1",
        )
        .bind(sid)
        .fetch_optional(&mut *conn)
        .await?;
        session = row.map(|(id, harness, project_key, started_at, device_id, hostname, client_kind)| SessionInfo {
            id,
            harness,
            project_key,
            device_id,
            hostname,
            client_kind,
            started_at,
        });
    }
    let event_seqs: Vec<i32> = if event_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_scalar("SELECT seq FROM events WHERE id = ANY($1) ORDER BY seq")
            .bind(event_ids.to_vec())
            .fetch_all(&mut *conn)
            .await?
    };
    let extracted_by = if extracted_by.is_null() { json!({}) } else { extracted_by };
    Ok(Provenance { session, event_ids: event_ids.to_vec(), event_seqs, extracted_by })
}

/// Provenance chain: the row, each evidence observation with its session (harness, project,
/// device) and event seqs, `extracted_by`, and the audit history (spec 12.5). Writes `access_log`.
pub async fn explain(state: &AppState, identity: &Identity, id: Uuid) -> Result<ExplainResponse, ApiError> {
    let mut tx = user_tx(&state.db.app, identity).await?;
    let item = load_items(&mut tx, &[id]).await?.remove(&id).ok_or_else(|| ApiError::not_found("memory not found"))?;
    let mut prov = None;
    let mut evidence = Vec::new();
    let evidence_ids: Vec<Uuid> = match &item {
        MemoryItem::Observations(o) => {
            prov = Some(provenance(&mut tx, o.session_id, &o.source_event_ids, o.extracted_by.clone()).await?);
            Vec::new()
        }
        MemoryItem::Summaries(s) => {
            prov = Some(provenance(&mut tx, s.session_id, &[], s.extracted_by.clone()).await?);
            Vec::new()
        }
        MemoryItem::Facts(f) => {
            if let Some(eb) = &f.extracted_by {
                prov = Some(Provenance { extracted_by: eb.clone(), ..Provenance::default() });
            }
            f.evidence_observation_ids.clone()
        }
        MemoryItem::Procedures(p) => p.source_observation_ids.clone(),
    };
    let mut ev_rows = hydrate(&mut tx, MemoryTable::Observations, &evidence_ids).await?;
    for oid in &evidence_ids {
        if let Some(TierRow::Obs(o)) = ev_rows.remove(oid) {
            let p = provenance(&mut tx, o.session_id, &o.source_event_ids, o.extracted_by).await?;
            evidence.push(EvidenceObservation {
                id: o.id,
                obs_type: o.obs_type,
                title: o.title,
                created_at: o.created_at,
                provenance: p,
            });
        }
    }
    type AuditRow = (
        Uuid,
        String,
        String,
        Option<Value>,
        Option<Value>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<Uuid>,
        DateTime<Utc>,
    );
    let audit: Vec<AuditRow> = sqlx::query_as(
        "SELECT id, actor, action, before, after, reason, model, prompt_version, run_id, created_at
           FROM memory_audit WHERE target_table = $1 AND target_id = $2 ORDER BY created_at, id",
    )
    .bind(item.table())
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    log_access(&mut tx, identity, "explain", &[&item]).await?;
    tx.commit().await?;
    let audit = audit
        .into_iter()
        .map(|(id, actor, action, before, after, reason, model, prompt_version, run_id, created_at)| AuditEntry {
            id,
            actor,
            action,
            before,
            after,
            reason,
            model,
            prompt_version,
            run_id,
            created_at,
        })
        .collect();
    Ok(ExplainResponse { id, table: item.table(), item, provenance: prov, evidence, audit })
}
