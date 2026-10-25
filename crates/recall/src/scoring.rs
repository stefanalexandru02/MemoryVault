//! Pure ranking math (spec 11.2-11.5): RRF, recency, final score, MMR and greedy budget fill.

use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use memvault_core::schemas::common::MemoryTable;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::candidates::TierRow;

pub const RRF_K: f64 = 60.0;
pub const MMR_LAMBDA: f64 = 0.7;
/// Procedures whose `task_class` equals `task_state.intent` (11.2).
pub const INTENT_BONUS: f64 = 0.3;
/// Type boost of a document chunk. Ingested docs are reference material, not what the user decided or said, so
/// at equal relevance they score 0.6 against about 0.73 for a typical fact (importance 0.5, confidence 0.95).
pub const DOCUMENT_WEIGHT: f64 = 0.8;

/// Half-lives in days (11.4). Facts, procedures and documents do not decay.
pub const HALF_LIFE_OBS: f64 = 30.0;
pub const HALF_LIFE_OBS_LONG: f64 = 90.0;
pub const HALF_LIFE_SUMMARIES: f64 = 60.0;

/// Type boosts (11.4 leaves the values open): decisions, bug fixes and security alerts are the
/// observations most often worth surfacing again.
pub fn type_boost(obs_type: &str) -> f64 {
    match obs_type {
        "decision" | "security_alert" => 1.2,
        "bugfix" => 1.1,
        _ => 1.0,
    }
}

pub fn half_life(table: MemoryTable, obs_type: Option<&str>) -> Option<f64> {
    match table {
        MemoryTable::Observations if matches!(obs_type, Some("decision" | "bugfix")) => Some(HALF_LIFE_OBS_LONG),
        MemoryTable::Observations => Some(HALF_LIFE_OBS),
        MemoryTable::Summaries => Some(HALF_LIFE_SUMMARIES),
        MemoryTable::Facts | MemoryTable::Procedures | MemoryTable::Documents => None,
    }
}

/// `0.5 ^ (age_days / half_life)`; 1 when the tier has no decay.
pub fn recency(age_days: f64, half_life: Option<f64>) -> f64 {
    half_life.map_or(1.0, |h| 0.5f64.powf(age_days.max(0.0) / h))
}

pub fn age_days(created_at: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    ((now - created_at).num_milliseconds() as f64 / 86_400_000.0).max(0.0)
}

/// Spec 11.4: `base × (0.5 + 0.5·importance) × (0.5 + 0.5·confidence_or_1) × (0.3 + 0.7·recency_or_1)
/// × type_boost`, plus an additive bonus (procedure intent match).
pub fn final_score(
    base: f64,
    importance: f64,
    confidence: Option<f64>,
    recency: Option<f64>,
    type_boost: f64,
    bonus: f64,
) -> f64 {
    let conf = confidence.unwrap_or(1.0);
    let rec = recency.unwrap_or(1.0);
    base * (0.5 + 0.5 * importance) * (0.5 + 0.5 * conf) * (0.3 + 0.7 * rec) * type_boost + bonus
}

/// One row of one tier as it moves through fusion, rerank and scoring.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: Uuid,
    pub table: MemoryTable,
    pub bm25: Option<f64>,
    pub cos: Option<f64>,
    pub rrf: f64,
    pub rerank: Option<f64>,
    pub score: f64,
    pub row: Option<TierRow>,
    pub embedding: Option<Vec<f32>>,
}

impl Candidate {
    pub fn new(id: Uuid, table: MemoryTable) -> Self {
        Self { id, table, bm25: None, cos: None, rrf: 0.0, rerank: None, score: 0.0, row: None, embedding: None }
    }

    /// Rerank score when present, else RRF (11.3).
    pub fn base(&self) -> f64 {
        self.rerank.unwrap_or(self.rrf)
    }

    /// Final score (11.4) from the hydrated row.
    pub fn compute_score(&self, now: DateTime<Utc>, intent: Option<&str>) -> f64 {
        let base = self.base();
        match &self.row {
            Some(TierRow::Obs(o)) => {
                let rec = recency(age_days(o.created_at, now), half_life(self.table, Some(&o.obs_type)));
                final_score(base, f64::from(o.importance), None, Some(rec), type_boost(&o.obs_type), 0.0)
            }
            Some(TierRow::Sum(s)) => {
                let rec = recency(age_days(s.created_at, now), half_life(self.table, None));
                final_score(base, 0.5, None, Some(rec), 1.0, 0.0)
            }
            Some(TierRow::Fact(f)) => {
                final_score(base, f64::from(f.importance), Some(f64::from(f.confidence)), None, 1.0, 0.0)
            }
            Some(TierRow::Proc(p)) => {
                let bonus = if intent.is_some_and(|i| i == p.task_class) { INTENT_BONUS } else { 0.0 };
                final_score(base, 0.5, None, None, 1.0, bonus)
            }
            Some(TierRow::Doc(_)) => final_score(base, 0.5, None, None, DOCUMENT_WEIGHT, 0.0),
            None => 0.0,
        }
    }

    /// Entry of `retrieval_log.candidates`.
    pub fn log(&self) -> Value {
        let round = |x: f64| (x * 1e6).round() / 1e6;
        let mut out = json!({ "id": self.id, "table": self.table, "rrf": round(self.rrf), "score": round(self.score) });
        for (k, v) in [("bm25", self.bm25), ("cos", self.cos), ("rerank", self.rerank)] {
            if let Some(v) = v {
                out[k] = json!(round(v));
            }
        }
        out
    }
}

/// Deterministic order: score descending, then id.
pub fn by_score(a: &Candidate, b: &Candidate) -> Ordering {
    b.score.total_cmp(&a.score).then(a.id.cmp(&b.id))
}

/// Reciprocal-rank fusion of two legs, each already ordered best first; the result is ordered by
/// RRF descending (then id) and cut at `limit`.
pub fn rrf_fuse(table: MemoryTable, keyword: &[(Uuid, f64)], vector: &[(Uuid, f64)], limit: usize) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let slot = |id: Uuid, out: &mut Vec<Candidate>| -> usize {
        out.iter().position(|c| c.id == id).unwrap_or_else(|| {
            out.push(Candidate::new(id, table));
            out.len() - 1
        })
    };
    for (rank, (id, s)) in keyword.iter().enumerate() {
        let i = slot(*id, &mut out);
        out[i].bm25 = Some(*s);
        out[i].rrf += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    for (rank, (id, s)) in vector.iter().enumerate() {
        let i = slot(*id, &mut out);
        out[i].cos = Some(*s);
        out[i].rrf += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    out.sort_by(|a, b| b.rrf.total_cmp(&a.rrf).then(a.id.cmp(&b.id)));
    out.truncate(limit);
    out
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        na += f64::from(*x) * f64::from(*x);
        nb += f64::from(*y) * f64::from(*y);
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
}

/// Maximal marginal relevance with λ = 0.7 over best-part embeddings (11.4), so paraphrases do not
/// fill a slot. Relevance is the score normalised to [0, 1] within the list; a candidate without an
/// embedding is dissimilar to everything. Ties keep score order.
pub fn mmr_order(mut cands: Vec<Candidate>) -> Vec<Candidate> {
    cands.sort_by(by_score);
    if cands.len() <= 1 || cands.iter().all(|c| c.embedding.is_none()) {
        return cands;
    }
    let top = cands.iter().map(|c| c.score).fold(0.0f64, f64::max);
    let rel: Vec<f64> = cands.iter().map(|c| if top > 0.0 { c.score / top } else { 0.0 }).collect();
    let mut max_sim = vec![0.0f64; cands.len()];
    let mut remaining: Vec<usize> = (0..cands.len()).collect();
    let mut order = Vec::with_capacity(cands.len());
    while !remaining.is_empty() {
        let (pos, &best) = remaining
            .iter()
            .enumerate()
            .max_by(|(_, i), (_, j)| {
                let (i, j) = (**i, **j);
                let mi = MMR_LAMBDA * rel[i] - (1.0 - MMR_LAMBDA) * max_sim[i];
                let mj = MMR_LAMBDA * rel[j] - (1.0 - MMR_LAMBDA) * max_sim[j];
                mi.total_cmp(&mj).then(j.cmp(&i))
            })
            .expect("non-empty");
        remaining.remove(pos);
        order.push(best);
        if let Some(e) = &cands[best].embedding {
            for &i in &remaining {
                if let Some(o) = &cands[i].embedding {
                    max_sim[i] = max_sim[i].max(cosine(e, o));
                }
            }
        }
    }
    let mut slots: Vec<Option<Candidate>> = cands.into_iter().map(Some).collect();
    order.into_iter().map(|i| slots[i].take().expect("each index once")).collect()
}

/// Greedy in order (11.5): take an item when it still fits, skip it otherwise. Returns the items and
/// the tokens used.
pub fn budget_fill<T>(items: Vec<T>, budget: u32, cost: impl Fn(&T) -> i32) -> (Vec<T>, i32) {
    let budget = i64::from(budget);
    let mut used = 0i64;
    let mut out = Vec::new();
    for item in items {
        let c = i64::from(cost(&item).max(0));
        if used + c <= budget {
            used += c;
            out.push(item);
        }
    }
    (out, i32::try_from(used).unwrap_or(i32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn rrf_rewards_both_legs() {
        let kw = [(id(1), 3.0), (id(2), 2.0)];
        let vec = [(id(2), 0.9), (id(3), 0.8)];
        let fused = rrf_fuse(MemoryTable::Observations, &kw, &vec, 40);
        assert_eq!(fused.iter().map(|c| c.id).collect::<Vec<_>>(), vec![id(2), id(1), id(3)]);
        assert!((fused[0].rrf - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-12);
        assert_eq!(fused[0].bm25, Some(2.0));
        assert_eq!(fused[0].cos, Some(0.9));
        assert_eq!(rrf_fuse(MemoryTable::Facts, &kw, &vec, 1).len(), 1);
    }

    #[test]
    fn score_formula() {
        assert!((recency(30.0, Some(30.0)) - 0.5).abs() < 1e-12);
        assert_eq!(recency(1000.0, None), 1.0);
        // base 1, importance 1, no confidence, fresh, no boost -> 1.
        assert!((final_score(1.0, 1.0, None, Some(1.0), 1.0, 0.0) - 1.0).abs() < 1e-12);
        let s = final_score(2.0, 0.5, Some(0.6), Some(0.5), 1.2, 0.3);
        let want = 2.0 * 0.75 * 0.8 * (0.3 + 0.35) * 1.2 + 0.3;
        assert!((s - want).abs() < 1e-12);
        assert_eq!(half_life(MemoryTable::Observations, Some("decision")), Some(90.0));
        assert_eq!(half_life(MemoryTable::Observations, Some("change")), Some(30.0));
        assert_eq!(half_life(MemoryTable::Facts, None), None);
    }

    #[test]
    fn mmr_demotes_near_duplicates() {
        let mk = |n: u128, score: f64, e: Vec<f32>| Candidate {
            score,
            embedding: Some(e),
            ..Candidate::new(id(n), MemoryTable::Observations)
        };
        let cands = vec![mk(1, 1.0, vec![1.0, 0.0]), mk(2, 0.95, vec![1.0, 0.01]), mk(3, 0.6, vec![0.0, 1.0])];
        let order: Vec<Uuid> = mmr_order(cands).into_iter().map(|c| c.id).collect();
        assert_eq!(order, vec![id(1), id(3), id(2)]);
    }

    #[test]
    fn mmr_without_embeddings_keeps_score_order() {
        let mk = |n: u128, score: f64| Candidate { score, ..Candidate::new(id(n), MemoryTable::Facts) };
        let order: Vec<Uuid> = mmr_order(vec![mk(2, 0.5), mk(1, 0.5), mk(3, 0.9)]).into_iter().map(|c| c.id).collect();
        assert_eq!(order, vec![id(3), id(1), id(2)]);
    }

    #[test]
    fn budget_fill_skips_what_does_not_fit() {
        let (got, used) = budget_fill(vec![50, 80, 30, 20], 100, |x| *x);
        assert_eq!(got, vec![50, 30, 20]);
        assert_eq!(used, 100);
        let (none, used) = budget_fill(vec![5], 0, |x| *x);
        assert!(none.is_empty());
        assert_eq!(used, 0);
    }
}
