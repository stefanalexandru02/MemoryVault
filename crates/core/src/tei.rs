//! Text Embeddings Inference client (spec 13) behind the [`Embedder`] trait, plus the
//! deterministic [`FakeTei`] used by tests and `FAKE_TEI=1`.
//!
//! Metrics: `memsvc_tei_latency_seconds{service}` (histogram) and `memsvc_tei_errors_total{service}`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::Settings;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TeiError {
    /// TEI did not answer within the configured timeout (rerank falls back to RRF, spec 11.3).
    #[error("{0}")]
    Timeout(String),
    /// TEI unreachable, returned an error status, or an unexpected body.
    #[error("{0}")]
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeiHealth {
    pub embed: bool,
    pub rerank: bool,
}

/// Embedding + rerank service. `AppState::tei` holds one as `Arc<dyn Embedder>`.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Value stored in `embeddings.model`.
    fn model(&self) -> &str;
    fn dims(&self) -> usize;
    /// Dense, L2-normalised vectors, one per text, in input order.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, TeiError>;
    /// Relevance score per text, aligned with `texts`.
    async fn rerank(&self, query: &str, texts: &[String]) -> Result<Vec<f32>, TeiError>;
    /// Whether `rerank` scores are a cross-encoder's relevance in 0 to 1, so that one threshold means the same for
    /// every query. False for [`FakeTei`], whose score is the share of the query's words found in the text.
    fn calibrated_rerank(&self) -> bool {
        true
    }
    async fn health(&self) -> TeiHealth;
}

/// `POST {embed_url}/embed` and `POST {rerank_url}/rerank` (HuggingFace TEI HTTP API).
#[derive(Debug, Clone)]
pub struct TeiClient {
    embed_url: String,
    rerank_url: String,
    model: String,
    dims: usize,
    embed_timeout: Duration,
    rerank_timeout: Duration,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct RerankHit {
    index: usize,
    score: f32,
}

impl TeiClient {
    pub fn new(
        embed_url: &str,
        rerank_url: &str,
        model: &str,
        dims: usize,
        embed_timeout: Duration,
        rerank_timeout: Duration,
    ) -> Self {
        Self {
            embed_url: embed_url.trim_end_matches('/').to_string(),
            rerank_url: rerank_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            dims,
            embed_timeout,
            rerank_timeout,
            http: reqwest::Client::new(),
        }
    }

    pub fn from_settings(s: &Settings) -> Self {
        Self::new(
            &s.tei_embed_url,
            &s.tei_rerank_url,
            &s.embed_model,
            s.embed_dims,
            s.tei_embed_timeout,
            s.tei_rerank_timeout,
        )
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        service: &'static str,
        url: String,
        body: serde_json::Value,
        timeout: Duration,
    ) -> Result<T, TeiError> {
        let start = Instant::now();
        let result = async {
            let resp = self.http.post(&url).json(&body).timeout(timeout).send().await?;
            resp.error_for_status()?.json::<T>().await
        }
        .await;
        metrics::histogram!("memsvc_tei_latency_seconds", "service" => service).record(start.elapsed().as_secs_f64());
        result.map_err(|e| {
            metrics::counter!("memsvc_tei_errors_total", "service" => service).increment(1);
            if e.is_timeout() {
                TeiError::Timeout(format!("{service} timed out after {timeout:?}"))
            } else {
                TeiError::Failed(format!("{service} failed: {e}"))
            }
        })
    }
}

#[async_trait]
impl Embedder for TeiClient {
    fn model(&self) -> &str {
        &self.model
    }

    fn dims(&self) -> usize {
        self.dims
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, TeiError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({ "inputs": texts, "truncate": true, "normalize": true });
        let vectors: Vec<Vec<f32>> =
            self.post("embed", format!("{}/embed", self.embed_url), body, self.embed_timeout).await?;
        if vectors.len() != texts.len() {
            return Err(TeiError::Failed("embed returned an unexpected shape".into()));
        }
        Ok(vectors)
    }

    async fn rerank(&self, query: &str, texts: &[String]) -> Result<Vec<f32>, TeiError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({ "query": query, "texts": texts, "truncate": true, "raw_scores": false });
        let hits: Vec<RerankHit> =
            self.post("rerank", format!("{}/rerank", self.rerank_url), body, self.rerank_timeout).await?;
        // TEI returns hits sorted by score; realign with the input order.
        let mut scores = vec![0.0; texts.len()];
        for hit in hits {
            *scores.get_mut(hit.index).ok_or_else(|| TeiError::Failed("rerank index out of range".into()))? = hit.score;
        }
        Ok(scores)
    }

    async fn health(&self) -> TeiHealth {
        let check = |url: String| async move {
            self.http.get(url).timeout(Duration::from_secs(1)).send().await.is_ok_and(|r| r.status().is_success())
        };
        let (embed, rerank) =
            tokio::join!(check(format!("{}/health", self.embed_url)), check(format!("{}/health", self.rerank_url)));
        TeiHealth { embed, rerank }
    }
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|t| !t.is_empty()).map(str::to_lowercase).collect()
}

/// Deterministic in-process stand-in (`FAKE_TEI=1`, tests).
///
/// `embed`: hashed bag of words (sha256 per lowercase word -> dimension and sign), L2-normalised,
/// so texts sharing words have higher cosine similarity. `rerank`: fraction of the query's
/// distinct words present in the text. `calls` records `(method, n_texts)` for assertions;
/// `set_down(true)` makes every call fail and `health` report down.
#[derive(Debug)]
pub struct FakeTei {
    model: String,
    dims: usize,
    pub calls: Mutex<Vec<(&'static str, usize)>>,
    down: Mutex<bool>,
}

impl FakeTei {
    pub fn new(model: &str, dims: usize) -> Self {
        Self { model: model.to_string(), dims, calls: Mutex::new(Vec::new()), down: Mutex::new(false) }
    }

    pub fn set_down(&self, down: bool) {
        *self.down.lock().expect("poisoned") = down;
    }

    fn is_down(&self) -> bool {
        *self.down.lock().expect("poisoned")
    }

    fn record(&self, method: &'static str, n: usize) -> Result<(), TeiError> {
        self.calls.lock().expect("poisoned").push((method, n));
        if self.is_down() { Err(TeiError::Failed(format!("{method} failed: fake TEI is down"))) } else { Ok(()) }
    }

    /// The vector `embed` returns for `text`.
    pub fn vector(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dims];
        let mut toks = tokens(text);
        if toks.is_empty() {
            toks.push("\0empty".into());
        }
        for tok in toks {
            let h = Sha256::digest(tok.as_bytes());
            let idx = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize % self.dims;
            v[idx] += if h[4] & 1 == 1 { 1.0 } else { -1.0 };
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }
}

impl Default for FakeTei {
    fn default() -> Self {
        Self::new("bge-m3", 1024)
    }
}

#[async_trait]
impl Embedder for FakeTei {
    fn model(&self) -> &str {
        &self.model
    }

    fn dims(&self) -> usize {
        self.dims
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, TeiError> {
        self.record("embed", texts.len())?;
        Ok(texts.iter().map(|t| self.vector(t)).collect())
    }

    async fn rerank(&self, query: &str, texts: &[String]) -> Result<Vec<f32>, TeiError> {
        self.record("rerank", texts.len())?;
        let mut q = tokens(query);
        q.sort();
        q.dedup();
        if q.is_empty() {
            return Ok(vec![0.0; texts.len()]);
        }
        Ok(texts
            .iter()
            .map(|t| {
                let words = tokens(t);
                q.iter().filter(|w| words.contains(w)).count() as f32 / q.len() as f32
            })
            .collect())
    }

    fn calibrated_rerank(&self) -> bool {
        false
    }

    async fn health(&self) -> TeiHealth {
        let up = !self.is_down();
        TeiHealth { embed: up, rerank: up }
    }
}

/// [`FakeTei`] when `FAKE_TEI=1`, else [`TeiClient`].
pub fn build_embedder(settings: &Settings) -> std::sync::Arc<dyn Embedder> {
    if settings.fake_tei {
        std::sync::Arc::new(FakeTei::new(&settings.embed_model, settings.embed_dims))
    } else {
        std::sync::Arc::new(TeiClient::from_settings(settings))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cos(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[tokio::test]
    async fn fake_tei_deterministic_and_lexical() {
        let tei = FakeTei::new("bge-m3", 1024);
        let texts: Vec<String> =
            ["deploy the memory api", "redeploy memory api after migration", "banana bread"].map(String::from).to_vec();
        let v = tei.embed(&texts).await.unwrap();
        assert_eq!(v[0].len(), 1024);
        assert!((v[0].iter().map(|x| x * x).sum::<f32>().sqrt() - 1.0).abs() < 1e-5);
        assert_eq!(FakeTei::default().embed(&texts[..1]).await.unwrap()[0], v[0]);
        assert!(cos(&v[0], &v[1]) > cos(&v[0], &v[2]));
        let scores = tei.rerank("memory api", &["the memory api", "api", "nothing"].map(String::from)).await.unwrap();
        assert_eq!(scores, vec![1.0, 0.5, 0.0]);
        assert_eq!(tei.health().await, TeiHealth { embed: true, rerank: true });
        assert_eq!(*tei.calls.lock().unwrap(), vec![("embed", 3), ("rerank", 3)]);
        tei.set_down(true);
        assert!(tei.embed(&texts).await.is_err());
        assert_eq!(tei.health().await, TeiHealth { embed: false, rerank: false });
    }
}
