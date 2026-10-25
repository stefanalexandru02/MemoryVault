//! `memvault evals replay`: rebuild a scratch space from recorded sessions, run the held-out
//! questions through recall and score the result (spec 16.2).
//!
//! The sessions go through the real ingest routes (sessions, events, work lease and result) of an
//! in-process router, so windows, packets, appliers and embedding jobs behave as in production.
//! Packet outputs come from the recording, or from a deterministic fake observer when the recording
//! has none (or `--observer fake`). With `--prompt name@version` the extract packets are pinned to
//! that prompt and answered by `claude -p`, so a new prompt version can be measured.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use memvault_core::auth::admin::{create_user, ensure_client};
use memvault_core::auth::scopes::TokenKind;
use memvault_core::auth::tokens::{MintOptions, mint_token};
use memvault_core::db::MIGRATOR;
use memvault_core::http::api_router;
use memvault_core::schemas::common::OutputSchemaName;
use memvault_core::schemas::prompts::PromptSpec;
use memvault_core::schemas::recall::RecallResponse;
use memvault_core::{AppState, AppliersRegistry, Db, system_tx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

use crate::session::{self, Line, RecordedSession};

/// A held-out question and what a good answer contains. Ids differ per replay, so expectations
/// name content: an observation title, a fact's subject and predicate, or summary text.
#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    pub id: String,
    pub query: String,
    pub expect: Vec<Expect>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Expect {
    /// Case-insensitive substring of an observation title.
    #[serde(default)]
    pub observation: Option<String>,
    #[serde(default)]
    pub fact: Option<FactRef>,
    /// Case-insensitive substring of a summary's request, learned, completed or next_steps.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FactRef {
    pub subject: String,
    pub predicate: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoldFact {
    pub subject: String,
    pub predicate: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
struct QuestionsFile {
    questions: Vec<Question>,
}

#[derive(Debug, Deserialize)]
struct GoldFile {
    #[serde(default)]
    facts: Vec<GoldFact>,
}

/// `baseline.json`: the recall@5 a change must not drop more than 5% below.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub recall_at_5: f64,
}

pub const MAX_RELATIVE_DROP: f64 = 0.05;

/// Everything under `evals/`.
#[derive(Debug, Clone)]
pub struct EvalSet {
    pub sessions: Vec<RecordedSession>,
    pub questions: Vec<Question>,
    pub gold: Vec<GoldFact>,
    pub baseline: Option<Baseline>,
}

impl EvalSet {
    pub fn load(dir: &Path) -> Result<Self> {
        let questions_path = dir.join("questions.yaml");
        let text = std::fs::read_to_string(&questions_path)
            .with_context(|| format!("reading {}", questions_path.display()))?;
        let questions = serde_yaml::from_str::<QuestionsFile>(&text)
            .with_context(|| format!("parsing {}", questions_path.display()))?
            .questions;
        let gold = match std::fs::read_to_string(dir.join("facts_gold.yaml")) {
            Ok(text) => serde_yaml::from_str::<GoldFile>(&text).context("parsing facts_gold.yaml")?.facts,
            Err(_) => Vec::new(),
        };
        let baseline = match std::fs::read_to_string(dir.join("baseline.json")) {
            Ok(text) => Some(serde_json::from_str(&text).context("parsing baseline.json")?),
            Err(_) => None,
        };
        let sessions = session::load_dir(&dir.join("sessions"))?;
        if sessions.is_empty() {
            bail!("no sessions in {}", dir.join("sessions").display());
        }
        Ok(Self { sessions, questions, gold, baseline })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Observer {
    /// Recorded outputs, the fake observer where the recording has none.
    #[default]
    Recorded,
    /// Ignore recorded outputs.
    Fake,
    /// Run the extract packets through `claude -p` (implied by `--prompt`); summaries keep using the
    /// recorded or fake outputs.
    Claude,
}

#[derive(Debug, Clone, Default)]
pub struct ReplayOptions {
    pub observer: Observer,
    pub rerank: bool,
    /// Pin the extract packets to this prompt (`observer.code@v4`) instead of the newest loaded one.
    /// Needs `Observer::Claude`: the recorded outputs belong to another prompt.
    pub prompt: Option<String>,
    /// The `claude` executable for `Observer::Claude` (default: `claude` from PATH).
    pub claude_bin: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuestionResult {
    pub id: String,
    pub recall_at_5: f64,
    /// 1-based rank of the first expected item among all recalled items, if any was recalled.
    pub first_hit_rank: Option<usize>,
    /// Token estimate of the items up to and including the first hit.
    pub tokens_to_hit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub questions: usize,
    pub recall_at_5: f64,
    pub first_hit_rank: Option<f64>,
    pub tokens_per_hit: Option<f64>,
    /// Share of extracted facts (active or candidate) that equal a gold fact; `None` without gold.
    pub fact_precision: Option<f64>,
    pub facts_extracted: usize,
    /// Groups of facts with the same subject, predicate and scope but different values.
    pub contradictions: i64,
    pub packets_failed: usize,
    pub per_question: Vec<QuestionResult>,
}

impl Report {
    pub fn print(&self) {
        let f = |v: Option<f64>, digits: usize| v.map_or("-".to_string(), |v| format!("{v:.digits$}"));
        println!("questions          {}", self.questions);
        println!("recall@5           {:.3}", self.recall_at_5);
        println!("first-hit rank     {}", f(self.first_hit_rank, 2));
        println!("tokens per hit     {}", f(self.tokens_per_hit, 1));
        println!("fact precision     {} ({} facts)", f(self.fact_precision, 3), self.facts_extracted);
        println!("contradictions     {}", self.contradictions);
        println!("packets failed     {}", self.packets_failed);
        for q in &self.per_question {
            println!(
                "  {:<20} recall@5 {:.2}  rank {}",
                q.id,
                q.recall_at_5,
                q.first_hit_rank.map_or("-".to_string(), |r| r.to_string())
            );
        }
    }
}

/// `Some(message)` when recall@5 dropped more than 5% (relative) below the baseline.
pub fn baseline_regression(report: &Report, baseline: &Baseline) -> Option<String> {
    if baseline.recall_at_5 <= 0.0 {
        return None;
    }
    let drop = (baseline.recall_at_5 - report.recall_at_5) / baseline.recall_at_5;
    (drop > MAX_RELATIVE_DROP).then(|| {
        format!(
            "recall@5 {:.3} is {:.1}% below the baseline {:.3} (limit {:.0}%)",
            report.recall_at_5,
            drop * 100.0,
            baseline.recall_at_5,
            MAX_RELATIVE_DROP * 100.0
        )
    })
}

// ---------- in-process API ----------

struct Api {
    app: Router,
    token: String,
}

impl Api {
    async fn call(&self, method: Method, uri: &str, body: Option<Value>) -> Result<(StatusCode, Value)> {
        let req =
            Request::builder().method(method).uri(uri).header(header::AUTHORIZATION, format!("Bearer {}", self.token));
        let req = match body {
            Some(v) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())),
            None => req.body(Body::empty()),
        }?;
        let resp = self.app.clone().oneshot(req).await.expect("infallible");
        let status = resp.status();
        let bytes = resp.into_body().collect().await?.to_bytes();
        let value =
            serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        Ok((status, value))
    }

    async fn post(&self, uri: &str, body: Value) -> Result<(StatusCode, Value)> {
        self.call(Method::POST, uri, Some(body)).await
    }
}

fn v1() -> Router<AppState> {
    memvault_ingest::router().merge(memvault_recall::router())
}

/// A state with the ingest appliers over `db`.
pub fn state_over(state: &AppState, db: Db) -> Result<AppState> {
    let mut appliers = AppliersRegistry::default();
    memvault_ingest::register_appliers(&mut appliers);
    AppState::new((*state.settings).clone(), db, appliers)
}

// ---------- replay ----------

/// Replay into the database behind `state`, which must run the ingest appliers (see
/// [`state_over`]). A fresh user and private space keep the replay apart from other data in it.
pub async fn replay(state: &AppState, set: &EvalSet, options: &ReplayOptions) -> Result<Report> {
    let pinned = check_prompt(state, options)?;
    let (space_id, token) = scratch_user(state).await?;
    let api = Api { app: api_router(v1()).with_state(state.clone()), token };

    let mut packets_failed = 0;
    for s in &set.sessions {
        packets_failed += replay_session(state, &api, s, options, pinned.as_ref()).await?;
    }
    for _ in 0..200 {
        if memvault_worker::embed_pass(state, "evals").await? == 0 {
            break;
        }
    }

    let mut per_question = Vec::new();
    for q in &set.questions {
        per_question.push(ask(&api, q, options.rerank).await?);
    }
    let hits: Vec<&QuestionResult> = per_question.iter().filter(|q| q.first_hit_rank.is_some()).collect();
    let mean = |it: Vec<f64>| (!it.is_empty()).then(|| it.iter().sum::<f64>() / it.len() as f64);
    let facts = extracted_facts(state, space_id).await?;
    Ok(Report {
        questions: per_question.len(),
        recall_at_5: mean(per_question.iter().map(|q| q.recall_at_5).collect()).unwrap_or(0.0),
        first_hit_rank: mean(hits.iter().filter_map(|q| q.first_hit_rank).map(|r| r as f64).collect()),
        tokens_per_hit: mean(hits.iter().filter_map(|q| q.tokens_to_hit).map(|t| t as f64).collect()),
        fact_precision: fact_precision(&facts.rows, &set.gold),
        facts_extracted: facts.rows.len(),
        contradictions: facts.contradictions,
        packets_failed,
        per_question,
    })
}

/// The prompt `--prompt` names, if it is loaded, produces observer output and runs with `claude`.
fn check_prompt(state: &AppState, options: &ReplayOptions) -> Result<Option<PromptSpec>> {
    let spec = match &options.prompt {
        Some(prompt_ref) => {
            if options.observer != Observer::Claude {
                bail!("--prompt {prompt_ref} needs the claude observer (recorded outputs belong to another prompt)");
            }
            let spec = state.prompts.get(prompt_ref, None).map_err(|e| anyhow::anyhow!("{e}"))?;
            if spec.output_schema != OutputSchemaName::ObserverV1 {
                bail!("{prompt_ref} produces {}, not observer.v1", spec.output_schema.as_str());
            }
            Some(spec.clone())
        }
        None if options.observer == Observer::Claude => bail!("--observer claude needs --prompt <name@version>"),
        None => None,
    };
    Ok(spec)
}

async fn scratch_user(state: &AppState) -> Result<(Uuid, String)> {
    let handle = format!("eval-{}", Uuid::now_v7().simple());
    let mut tx = system_tx(&state.db.system).await?;
    let user = create_user(&mut tx, &handle, None).await?;
    let kind = TokenKind::Device;
    let client = ensure_client(&mut tx, user.user_id, "evals", kind.client_kind(), None, None).await?;
    let minted =
        mint_token(&mut tx, user.user_id, client, kind, &state.settings.token_pepper, MintOptions::default()).await?;
    tx.commit().await?;
    Ok((user.space_id, minted.token))
}

/// Post one recorded session and settle its packets. Returns the number of packets that failed.
async fn replay_session(
    state: &AppState,
    api: &Api,
    s: &RecordedSession,
    options: &ReplayOptions,
    pinned: Option<&PromptSpec>,
) -> Result<usize> {
    let (status, created) = api
        .post(
            "/v1/sessions",
            json!({
                "harness": s.harness, "external_session_id": s.external_session_id,
                "project_key": s.project_key, "cwd": s.cwd,
            }),
        )
        .await?;
    if !status.is_success() {
        bail!("creating session {}: {status} {created}", s.external_session_id);
    }
    let session_id = created["session_id"].as_str().context("session_id")?.to_string();

    let events: Vec<Value> = s
        .events
        .iter()
        .filter_map(|e| match e {
            Line::Event { seq, kind, occurred_at, payload } => Some(json!({
                "seq": seq, "kind": kind, "occurred_at": occurred_at, "payload": payload,
                "idempotency_key": format!("{session_id}:{seq}"),
            })),
            _ => None,
        })
        .collect();
    for chunk in events.chunks(100) {
        let (status, body) = api.post(&format!("/v1/sessions/{session_id}/events"), json!({ "events": chunk })).await?;
        if status != StatusCode::ACCEPTED {
            bail!("posting events of {}: {status} {body}", s.external_session_id);
        }
    }
    let (status, body) = api.post(&format!("/v1/sessions/{session_id}/end"), json!({"reason": "eval"})).await?;
    if !status.is_success() {
        bail!("ending session {}: {status} {body}", s.external_session_id);
    }
    if let Some(spec) = pinned {
        // Every extract packet exists once the session has ended; lease renders the prompt it is pinned to.
        let mut tx = system_tx(&state.db.system).await?;
        sqlx::query(
            "UPDATE work_packets SET prompt_ref = $2, model_hint = $3, output_schema = $4
              WHERE session_id = $1 AND kind = 'extract' AND status = 'pending'",
        )
        .bind(Uuid::parse_str(&session_id)?)
        .bind(spec.prompt_ref())
        .bind(&spec.model_hint)
        .bind(spec.output_schema.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }

    let mut failed = 0;
    // Summaries are created as the packets they summarise settle, so lease until nothing is left.
    for _ in 0..200 {
        let (status, leased) = api.post("/v1/work/lease", json!({"session_id": session_id, "max": 10})).await?;
        if !status.is_success() {
            bail!("leasing packets: {status} {leased}");
        }
        let packets = leased["packets"].as_array().cloned().unwrap_or_default();
        if packets.is_empty() {
            return Ok(failed);
        }
        for p in packets {
            let id = p["id"].as_str().context("packet id")?;
            let output = match options.observer {
                Observer::Recorded => recorded_output(s, &p),
                Observer::Fake => None,
                Observer::Claude if p["kind"] == "extract" => match model_output(options, &p).await {
                    Ok(output) => Some(output),
                    Err(e) => {
                        tracing::warn!(packet = id, error = %e, "claude run failed");
                        failed += 1;
                        api.post(&format!("/v1/work/{id}/fail"), json!({"error": "other", "detail": e.to_string()}))
                            .await?;
                        continue;
                    }
                },
                Observer::Claude => recorded_output(s, &p),
            }
            .unwrap_or_else(|| fake_output(&p));
            let (status, body) = api.post(&format!("/v1/work/{id}/result"), json!({"output": output})).await?;
            if !status.is_success() {
                tracing::warn!(packet = id, %status, %body, "result rejected");
                failed += 1;
                // The packet goes back to the queue with a backoff, so the lease loop ends.
                api.post(&format!("/v1/work/{id}/fail"), json!({"error": "other", "detail": body.to_string()})).await?;
            }
        }
    }
    bail!("session {} still has packets after 200 rounds", s.external_session_id)
}

/// What the model answers for a leased packet: its JSON object, or the raw text for the server to
/// unwrap (a code fence) or reject.
async fn model_output(options: &ReplayOptions, packet: &Value) -> Result<Value> {
    let bin = options.claude_bin.as_deref().unwrap_or("claude");
    let model = packet["model_hint"].as_str().filter(|m| !m.is_empty()).unwrap_or("sonnet");
    let system = packet["prompt"]["system"].as_str().unwrap_or_default();
    let user = packet["prompt"]["user"].as_str().unwrap_or_default();
    let text = crate::claude::run(bin, model, system, user).await?;
    Ok(match serde_json::from_str::<Value>(text.trim()) {
        Ok(v) if v.is_object() => v,
        _ => Value::String(text),
    })
}

fn recorded_output(s: &RecordedSession, packet: &Value) -> Option<Value> {
    let kind = packet["kind"].as_str()?;
    s.outputs.iter().find_map(|l| match l {
        Line::Output { packet: k, window_no, prompt_no, output } if k == kind => {
            let matches = match kind {
                "extract" => *window_no == packet["inputs"]["window_no"].as_i64(),
                "summarize_prompt" => *prompt_no == packet["inputs"]["prompt_no"].as_i64(),
                _ => true,
            };
            matches.then(|| output.clone())
        }
        _ => None,
    })
}

// ---------- fake observer ----------

fn clip(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(max).collect()
}

/// A deterministic stand-in for the model: one observation per window from the prompt and the tool
/// calls, and plain summaries built from the packet inputs.
fn fake_output(packet: &Value) -> Value {
    let inputs = &packet["inputs"];
    match packet["kind"].as_str().unwrap_or_default() {
        "extract" => fake_extract(inputs),
        "summarize_prompt" => {
            let titles: Vec<&str> = inputs["observation_titles"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["title"].as_str())
                .collect();
            json!({
                "request": clip(inputs["prompt"].as_str().unwrap_or("(no prompt)"), 200),
                "completed": titles.join("; "),
                "next_steps": "Continue from the last change.",
            })
        }
        _ => {
            let sums = inputs["prompt_summaries"].as_array().cloned().unwrap_or_default();
            let field = |name: &str| {
                sums.iter().filter_map(|s| s[name].as_str()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("; ")
            };
            json!({
                "request": clip(&field("request"), 300),
                "completed": clip(&field("completed"), 600),
                "next_steps": "Continue from the last change.",
            })
        }
    }
}

fn fake_extract(inputs: &Value) -> Value {
    let events = inputs["events"].as_array().cloned().unwrap_or_default();
    let prompt = events.iter().find(|e| e["kind"] == "user_prompt").and_then(|e| e["text"].as_str());
    let tools: Vec<&Value> = events.iter().filter(|e| e["kind"] == "tool_use").collect();
    let files: Vec<String> = tools
        .iter()
        .filter_map(|e| e["input"]["file_path"].as_str().or_else(|| e["input"]["path"].as_str()))
        .map(String::from)
        .collect();
    let edited = tools.iter().any(|e| matches!(e["tool"].as_str(), Some("Edit" | "Write" | "MultiEdit")));
    let title = match (prompt, files.first()) {
        (Some(p), _) => clip(p, 100),
        (None, Some(f)) => clip(&format!("Work on {f}"), 100),
        (None, None) => "Session activity".to_string(),
    };
    let narrative = events
        .iter()
        .rev()
        .find(|e| e["kind"] == "assistant_message")
        .and_then(|e| e["text"].as_str())
        .map(|t| clip(t, 600))
        .unwrap_or_else(|| title.clone());
    let mut seqs: Vec<i64> =
        events.iter().filter(|e| e["kind"] != "user_prompt").filter_map(|e| e["seq"].as_i64()).take(3).collect();
    if seqs.is_empty() {
        seqs = events.iter().filter_map(|e| e["seq"].as_i64()).take(1).collect();
    }
    json!({
        "skip": false,
        "observations": [{
            "type": if edited { "change" } else { "discovery" },
            "title": title, "narrative": narrative, "facts": [],
            "concepts": ["what-changed"], "files": files, "importance": 0.5,
            "source_event_seqs": seqs,
        }],
        "fact_candidates": [],
    })
}

// ---------- questions ----------

struct Ranked {
    kind: &'static str,
    text: String,
    subject: String,
    predicate: String,
    score: f64,
    tokens: i64,
}

fn ranked_items(resp: &RecallResponse) -> Vec<Ranked> {
    let s = &resp.slots;
    let mut items = Vec::new();
    for o in &s.observations {
        let text = o.title.to_lowercase();
        items.push(Ranked {
            kind: "observation",
            text,
            subject: String::new(),
            predicate: String::new(),
            score: o.item.score,
            tokens: o.item.token_estimate as i64,
        });
    }
    for f in &s.facts {
        items.push(Ranked {
            kind: "fact",
            text: f.value_text.to_lowercase(),
            subject: f.subject.to_lowercase(),
            predicate: f.predicate.to_lowercase(),
            score: f.item.score,
            tokens: f.item.token_estimate as i64,
        });
    }
    for m in &s.summaries {
        let text = [&m.request, &m.learned, &m.completed, &m.next_steps]
            .iter()
            .filter_map(|v| v.as_deref())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        items.push(Ranked {
            kind: "summary",
            text,
            subject: String::new(),
            predicate: String::new(),
            score: m.item.score,
            tokens: m.item.token_estimate as i64,
        });
    }
    items.sort_by(|a, b| b.score.total_cmp(&a.score));
    items
}

fn matches(e: &Expect, r: &Ranked) -> bool {
    if let Some(o) = &e.observation {
        return r.kind == "observation" && r.text.contains(&o.to_lowercase());
    }
    if let Some(f) = &e.fact {
        return r.kind == "fact" && r.subject == f.subject.to_lowercase() && r.predicate == f.predicate.to_lowercase();
    }
    if let Some(s) = &e.summary {
        return r.kind == "summary" && r.text.contains(&s.to_lowercase());
    }
    false
}

async fn ask(api: &Api, q: &Question, rerank: bool) -> Result<QuestionResult> {
    let (status, body) = api.post("/v1/recall", json!({"query": q.query, "rerank": rerank, "format": "json"})).await?;
    if !status.is_success() {
        bail!("recall for question {}: {status} {body}", q.id);
    }
    let resp: RecallResponse = serde_json::from_value(body).context("parsing the recall response")?;
    let items = ranked_items(&resp);
    let top5 = &items[..items.len().min(5)];
    let found = q.expect.iter().filter(|e| top5.iter().any(|r| matches(e, r))).count();
    let first = items.iter().position(|r| q.expect.iter().any(|e| matches(e, r)));
    Ok(QuestionResult {
        id: q.id.clone(),
        recall_at_5: if q.expect.is_empty() { 1.0 } else { found as f64 / q.expect.len() as f64 },
        first_hit_rank: first.map(|i| i + 1),
        tokens_to_hit: first.map(|i| items[..=i].iter().map(|r| r.tokens).sum()),
    })
}

// ---------- facts ----------

struct FactRow {
    subject: String,
    predicate: String,
    value_text: String,
}

struct Facts {
    rows: Vec<FactRow>,
    contradictions: i64,
}

async fn extracted_facts(state: &AppState, space_id: Uuid) -> Result<Facts> {
    let mut tx = system_tx(&state.db.system).await?;
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT e.canonical_name, f.predicate, f.value_text
           FROM facts f JOIN entities e ON e.id = f.subject_id
          WHERE f.space_id = $1 AND f.status IN ('active','candidate')",
    )
    .bind(space_id)
    .fetch_all(&mut *tx)
    .await?;
    let contradictions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
           SELECT 1 FROM facts
            WHERE space_id = $1 AND status IN ('active','candidate')
            GROUP BY subject_id, predicate, scope_kind, scope_key
           HAVING count(DISTINCT lower(btrim(value_text))) > 1) g",
    )
    .bind(space_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok(Facts {
        rows: rows
            .into_iter()
            .map(|(subject, predicate, value_text)| FactRow { subject, predicate, value_text })
            .collect(),
        contradictions,
    })
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Facts that equal a gold fact (subject, predicate, value; case-insensitive) over all facts.
/// Facts the gold file does not list count as wrong.
fn fact_precision(facts: &[FactRow], gold: &[GoldFact]) -> Option<f64> {
    if gold.is_empty() {
        return None;
    }
    if facts.is_empty() {
        return Some(0.0);
    }
    let right = facts
        .iter()
        .filter(|f| {
            gold.iter().any(|g| {
                norm(&g.subject) == norm(&f.subject)
                    && norm(&g.predicate) == norm(&f.predicate)
                    && norm(&g.value) == norm(&f.value_text)
            })
        })
        .count();
    Some(right as f64 / facts.len() as f64)
}

// ---------- scratch database ----------

/// A throwaway database named `memvault_eval_<id>`, migrated, dropped by [`Scratch::drop_db`].
pub struct Scratch {
    pub state: AppState,
    admin: PgPool,
    name: String,
}

impl Scratch {
    /// Create the database next to the one in `DATABASE_URL`. Needs CREATEDB.
    pub async fn create(state: &AppState) -> Result<Self> {
        let base = PgConnectOptions::from_str(&state.settings.database_url).context("DATABASE_URL")?;
        let admin = PgPoolOptions::new().max_connections(1).connect_with(base.clone()).await?;
        let name = format!("memvault_eval_{}", Uuid::now_v7().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .context("creating the scratch database")?;
        let pool = PgPoolOptions::new().max_connections(8).connect_with(base.database(&name)).await?;
        MIGRATOR.run(&pool).await.context("migrating the scratch database")?;
        let state = state_over(state, Db::single(pool))?;
        Ok(Self { state, admin, name })
    }

    pub async fn drop_db(self) -> Result<()> {
        self.state.db.app.close().await;
        sqlx::query(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name)).execute(&self.admin).await?;
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}
