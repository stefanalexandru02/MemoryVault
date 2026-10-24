//! Applying an `extract` result (spec 10.4), in the same transaction that marks the packet done.

use std::collections::{BTreeSet, HashMap};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use memvault_core::ids::new_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::jobs::enqueue_embed;
use memvault_core::schemas::common::{ScopeHint, ScopeKind};
use memvault_core::schemas::outputs::{FactCandidate, LlmOutput, ObservedObservation, ObserverV1};
use memvault_core::schemas::work::PacketRecord;
use memvault_core::{ApiError, AppliedCounts, PacketApplier, estimate_tokens};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::builders::{self, SessionRow, to_json};

/// Duplicate observations are looked for within this window.
const DEDUP_WINDOW: &str = "24 hours";

pub type ScopeRef = (ScopeKind, String);

// ---------- helpers shared with the summary appliers ----------

pub(crate) async fn packet_session(conn: &mut PgConnection, packet: &PacketRecord) -> Result<SessionRow, ApiError> {
    let session_id = packet
        .session_id
        .ok_or_else(|| ApiError::internal(format!("{} packet {} has no session", packet.kind, packet.id)))?;
    builders::session_row(conn, session_id).await?.ok_or_else(|| ApiError::internal("packet session not found"))
}

fn meta_str(packet: &PacketRecord, key: &str) -> Option<String> {
    packet.result_meta.as_ref()?.get(key)?.as_str().map(String::from)
}

/// `extracted_by` of rows derived from a packet (spec 4.3).
pub fn extracted_by(packet: &PacketRecord) -> Value {
    json!({
        "runner": meta_str(packet, "runner"),
        "runner_kind": meta_str(packet, "runner_kind"),
        "model": meta_str(packet, "model"),
        "prompt": packet.prompt_ref,
        "packet_id": packet.id,
    })
}

/// Scope of a derived row: project by default; `device` / `global` when the prompt says so (10.4).
/// A session without a project key falls back to its device.
pub fn scope_for(hint: Option<ScopeHint>, session: &SessionRow) -> ScopeRef {
    match (hint, session.project_key.as_deref()) {
        (Some(ScopeHint::Global), _) => (ScopeKind::Global, String::new()),
        (Some(ScopeHint::Device), _) | (_, None | Some("")) => (ScopeKind::Device, session.device_id.clone()),
        (_, Some(project)) => (ScopeKind::Project, project.to_string()),
    }
}

/// One `memory_audit` row with actor `packet:<id>`.
#[derive(Debug, Default)]
pub struct Audit<'a> {
    pub action: &'a str,
    pub table: &'a str,
    pub target_id: Uuid,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub reason: Option<&'a str>,
}

pub async fn audit(conn: &mut PgConnection, packet: &PacketRecord, a: Audit<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO memory_audit (id, space_id, actor, action, target_table, target_id, before, after, reason,
                                   model, prompt_version)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(new_id())
    .bind(packet.space_id)
    .bind(format!("packet:{}", packet.id))
    .bind(a.action)
    .bind(a.table)
    .bind(a.target_id)
    .bind(a.before)
    .bind(a.after)
    .bind(a.reason)
    .bind(meta_str(packet, "model"))
    .bind(&packet.prompt_ref)
    .execute(conn)
    .await?;
    Ok(())
}

// ---------- observations ----------

pub fn content_hash(obs: &ObservedObservation) -> String {
    let mut parts = vec![obs.obs_type.as_str(), obs.title.as_str()];
    parts.extend(obs.facts.iter().map(String::as_str));
    hex::encode(Sha256::digest(parts.join("|").as_bytes()))
}

/// Lowercase; punctuation, symbols and emoji become spaces; whitespace collapsed (claude-mem's rule).
pub fn normalize_title(title: &str) -> String {
    let kept: String =
        title.to_lowercase().chars().map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' }).collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Key for the near-duplicate check, unique per (scope, platform, agent-ness, normalized title).
/// `None` when the title normalizes to nothing.
pub fn title_norm_key(title: &str, scope: &ScopeRef, platform: &str, subagent: bool) -> Option<String> {
    let norm = normalize_title(title);
    if norm.is_empty() {
        return None;
    }
    let scope_text = format!("{}:{}", scope.0.as_str(), scope.1);
    let raw = [scope_text.as_str(), platform, if subagent { "subagent" } else { "main" }, norm.as_str()].join("\0");
    let mut key = hex::encode(Sha256::digest(raw.as_bytes()));
    key.truncate(32);
    Some(key)
}

struct NewObservation<'a> {
    obs: &'a ObservedObservation,
    scope: &'a ScopeRef,
    content_hash: &'a str,
    norm_key: Option<&'a str>,
    event_ids: &'a HashMap<i64, Uuid>,
}

async fn insert_observation(
    conn: &mut PgConnection,
    packet: &PacketRecord,
    session: &SessionRow,
    n: NewObservation<'_>,
) -> Result<Uuid, sqlx::Error> {
    let obs = n.obs;
    let id = new_id();
    let mut body =
        vec![obs.title.as_str(), obs.subtitle.as_deref().unwrap_or(""), obs.narrative.as_deref().unwrap_or("")];
    body.extend(obs.facts.iter().map(String::as_str));
    let mut source_ids: Vec<Uuid> = Vec::new();
    for seq in &obs.source_event_seqs {
        if let Some(id) = n.event_ids.get(seq).filter(|id| !source_ids.contains(id)) {
            source_ids.push(*id);
        }
    }
    let window_no = packet.inputs.get("window_no").and_then(Value::as_i64).and_then(|w| i32::try_from(w).ok());
    sqlx::query(
        "INSERT INTO observations (id, space_id, scope_kind, scope_key, session_id, window_no, type, title, subtitle,
                                   narrative, facts, concepts, entities, files, importance, sensitivity,
                                   source_event_ids, extracted_by, content_hash, title_norm_key, token_estimate)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)",
    )
    .bind(id)
    .bind(packet.space_id)
    .bind(n.scope.0)
    .bind(&n.scope.1)
    .bind(session.id)
    .bind(window_no)
    .bind(obs.obs_type)
    .bind(&obs.title)
    .bind(&obs.subtitle)
    .bind(&obs.narrative)
    .bind(&obs.facts)
    .bind(&obs.concepts)
    .bind(&obs.entities)
    .bind(&obs.files)
    .bind(obs.importance as f32)
    .bind(obs.sensitivity)
    .bind(&source_ids)
    .bind(extracted_by(packet))
    .bind(n.content_hash)
    .bind(n.norm_key)
    .bind(estimate_tokens(&body.join(" ")))
    .execute(&mut *conn)
    .await?;
    let after = json!({"type": obs.obs_type, "title": obs.title});
    audit(
        conn,
        packet,
        Audit { action: "obs.create", table: "observations", target_id: id, after: Some(after), ..Default::default() },
    )
    .await?;
    enqueue_embed(conn, "observations", id).await?;
    Ok(id)
}

/// An existing observation this one duplicates (spec 10.4 step 2): the same `content_hash` in the
/// space within 24 h (its importance is raised to the max of both), else the same `title_norm_key`
/// within 24 h (audited `obs.merge`).
async fn find_duplicate(
    conn: &mut PgConnection,
    packet: &PacketRecord,
    obs: &ObservedObservation,
    content_hash: &str,
    norm_key: Option<&str>,
) -> Result<Option<Uuid>, sqlx::Error> {
    let same_hash: Option<Uuid> = sqlx::query_scalar(&format!(
        "UPDATE observations SET importance = greatest(importance, $3)
          WHERE id = (SELECT id FROM observations WHERE space_id = $1 AND content_hash = $2
                        AND created_at > now() - interval '{DEDUP_WINDOW}' ORDER BY created_at LIMIT 1)
         RETURNING id"
    ))
    .bind(packet.space_id)
    .bind(content_hash)
    .bind(obs.importance as f32)
    .fetch_optional(&mut *conn)
    .await?;
    if same_hash.is_some() {
        return Ok(same_hash);
    }
    let Some(norm_key) = norm_key else {
        return Ok(None);
    };
    let same_title: Option<Uuid> = sqlx::query_scalar(&format!(
        "SELECT id FROM observations WHERE space_id = $1 AND title_norm_key = $2 AND archived_at IS NULL
            AND created_at > now() - interval '{DEDUP_WINDOW}' ORDER BY created_at LIMIT 1"
    ))
    .bind(packet.space_id)
    .bind(norm_key)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = same_title {
        audit(
            conn,
            packet,
            Audit {
                action: "obs.merge",
                table: "observations",
                target_id: id,
                after: Some(to_json(obs)),
                reason: Some("same normalized title within 24 h; new observation skipped"),
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(same_title)
}

// ---------- fact candidates ----------

/// The entity of `kind` whose canonical name or an alias matches `name` (case-insensitive), else a
/// new one (embedded).
pub async fn resolve_entity(
    conn: &mut PgConnection,
    space_id: Uuid,
    kind: &str,
    name: &str,
) -> Result<Uuid, sqlx::Error> {
    let name = name.trim();
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM entities
          WHERE space_id = $1 AND kind = $2
            AND (lower(canonical_name) = lower($3) OR EXISTS (SELECT 1 FROM unnest(aliases) a WHERE lower(a) = lower($3)))
          ORDER BY (lower(canonical_name) = lower($3)) DESC, created_at LIMIT 1",
    )
    .bind(space_id)
    .bind(kind)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = found {
        return Ok(id);
    }
    let (id, inserted): (Uuid, bool) = sqlx::query_as(
        "INSERT INTO entities (id, space_id, kind, canonical_name) VALUES ($1, $2, $3, $4)
         ON CONFLICT (space_id, kind, canonical_name) DO UPDATE SET canonical_name = EXCLUDED.canonical_name
         RETURNING id, (xmax = 0)",
    )
    .bind(new_id())
    .bind(space_id)
    .bind(kind)
    .bind(name)
    .fetch_one(&mut *conn)
    .await?;
    if inserted {
        enqueue_embed(conn, "entities", id).await?;
    }
    Ok(id)
}

#[derive(Debug, sqlx::FromRow)]
struct ActiveFact {
    id: Uuid,
    value: Value,
    value_text: String,
    confidence: f32,
    confirmations: i32,
    pinned: bool,
}

/// Same value: jsonb equality, else equal `value_text` ignoring case and surrounding whitespace.
/// (The spec's embedding-cosine > 0.95 test needs the embedder, which appliers do not get; such
/// near-duplicates land as conflict candidates and consolidation merges them.)
fn same_value(active: &ActiveFact, cand: &FactCandidate) -> bool {
    active.value == cand.value || active.value_text.trim().to_lowercase() == cand.value_text.trim().to_lowercase()
}

/// RFC 3339, a naive date-time (taken as UTC) or a date.
pub fn parse_valid_from(value: Option<&str>) -> Option<DateTime<Utc>> {
    let v = value?.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(v) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(dt.and_utc());
    }
    NaiveDate::parse_from_str(v, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)).map(|dt| dt.and_utc())
}

struct NewCandidate<'a> {
    cand: &'a FactCandidate,
    scope: &'a ScopeRef,
    subject_id: Uuid,
    evidence: &'a [Uuid],
    supersedes: Option<Uuid>,
}

async fn insert_candidate(
    conn: &mut PgConnection,
    packet: &PacketRecord,
    session: &SessionRow,
    n: NewCandidate<'_>,
) -> Result<Uuid, sqlx::Error> {
    let id = new_id();
    let value = if n.cand.value.is_null() { Value::String(n.cand.value_text.clone()) } else { n.cand.value.clone() };
    sqlx::query(
        "INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text, confidence,
                            status, conflict, supersedes_id, valid_from, evidence_observation_ids,
                            learned_on_client_ids, extracted_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'candidate', $10, $11, $12, $13, $14, $15)",
    )
    .bind(id)
    .bind(packet.space_id)
    .bind(n.scope.0)
    .bind(&n.scope.1)
    .bind(n.subject_id)
    .bind(&n.cand.predicate)
    .bind(value)
    .bind(&n.cand.value_text)
    .bind(n.cand.confidence as f32)
    .bind(n.supersedes.is_some())
    .bind(n.supersedes)
    .bind(parse_valid_from(n.cand.valid_from.as_deref()))
    .bind(n.evidence)
    .bind(vec![session.client_id])
    .bind(extracted_by(packet))
    .execute(&mut *conn)
    .await?;
    enqueue_embed(conn, "facts", id).await?;
    Ok(id)
}

/// Spec 10.4 step 3, same value: `confirmations += 1`, `confidence = 1 - (1-c_old)*(1-c_new*0.5)`
/// (unchanged when pinned), evidence and `learned_on_client_ids` merged, audited `fact.confirm`.
async fn confirm(
    conn: &mut PgConnection,
    packet: &PacketRecord,
    session: &SessionRow,
    active: &ActiveFact,
    cand: &FactCandidate,
    evidence: &[Uuid],
) -> Result<(), sqlx::Error> {
    let c_old = f64::from(active.confidence);
    let confidence = if active.pinned { c_old } else { 1.0 - (1.0 - c_old) * (1.0 - cand.confidence * 0.5) };
    let (new_confidence, confirmations): (f32, i32) = sqlx::query_as(
        "UPDATE facts SET confirmations = confirmations + 1, last_confirmed_at = now(), confidence = $2,
                evidence_observation_ids = ARRAY(SELECT DISTINCT unnest(evidence_observation_ids || $3::uuid[])),
                learned_on_client_ids = ARRAY(SELECT DISTINCT unnest(learned_on_client_ids || $4::uuid[]))
          WHERE id = $1 RETURNING confidence, confirmations",
    )
    .bind(active.id)
    .bind(confidence.min(1.0) as f32)
    .bind(evidence)
    .bind(vec![session.client_id])
    .fetch_one(&mut *conn)
    .await?;
    audit(
        conn,
        packet,
        Audit {
            action: "fact.confirm",
            table: "facts",
            target_id: active.id,
            before: Some(json!({"confidence": active.confidence, "confirmations": active.confirmations})),
            after: Some(json!({
                "confidence": new_confidence, "confirmations": confirmations, "value_text": cand.value_text,
            })),
            ..Default::default()
        },
    )
    .await
}

// ---------- the applier ----------

/// `extract` packets (`observer.v1`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractApplier;

#[async_trait]
impl PacketApplier for ExtractApplier {
    async fn apply(
        &self,
        conn: &mut PgConnection,
        packet: &PacketRecord,
        output: &LlmOutput,
    ) -> Result<AppliedCounts, ApiError> {
        let LlmOutput::Observer(output) = output else {
            return Err(ApiError::internal(format!("extract packet got a {} output", output.schema())));
        };
        apply_extract(conn, packet, output).await
    }
}

/// Spec 8.3: observations citing events a subagent produced carry `subagent:<agent_type>` in
/// `entities`, whatever the model wrote. The agent types come from the packet's immutable
/// `inputs.events` (see [`crate::builders::packet_event`]).
pub fn tag_subagent_observations(observations: &[ObservedObservation], inputs: &Value) -> Vec<ObservedObservation> {
    let agent_types: HashMap<i64, &str> = inputs
        .get("events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e.get("seq")?.as_i64()?, e.get("agent_type")?.as_str()?)))
        .filter(|(_, t)| !t.is_empty())
        .collect();
    observations
        .iter()
        .map(|obs| {
            let mut obs = obs.clone();
            for seq in &obs.source_event_seqs {
                if let Some(t) = agent_types.get(seq) {
                    let tag = format!("subagent:{t}");
                    if !obs.entities.contains(&tag) {
                        obs.entities.push(tag);
                    }
                }
            }
            obs
        })
        .collect()
}

pub async fn apply_extract(
    conn: &mut PgConnection,
    packet: &PacketRecord,
    output: &ObserverV1,
) -> Result<AppliedCounts, ApiError> {
    let mut counts: AppliedCounts =
        ["observations", "duplicates", "fact_candidates", "fact_confirmations", "procedure_hints"]
            .into_iter()
            .map(|k| (k.to_string(), 0))
            .collect();
    if output.skip {
        return Ok(counts);
    }
    let session = packet_session(conn, packet).await?;
    let mut touched: BTreeSet<ScopeRef> = BTreeSet::new();

    let seqs: Vec<i32> = output
        .observations
        .iter()
        .flat_map(|o| o.source_event_seqs.iter())
        .chain(output.fact_candidates.iter().flat_map(|f| f.evidence_seqs.iter()))
        .filter_map(|s| i32::try_from(*s).ok())
        .collect();
    let rows: Vec<(i32, Uuid)> = sqlx::query_as("SELECT seq, id FROM events WHERE session_id = $1 AND seq = ANY($2)")
        .bind(session.id)
        .bind(&seqs)
        .fetch_all(&mut *conn)
        .await?;
    let event_ids: HashMap<i64, Uuid> = rows.into_iter().map(|(s, id)| (i64::from(s), id)).collect();

    // observations; seq -> observations (new or matched) citing that event, for fact evidence
    let mut obs_by_seq: HashMap<i64, Vec<Uuid>> = HashMap::new();
    let mut packet_obs: Vec<Uuid> = Vec::new();
    let mut first_new: Option<Uuid> = None;
    let observations = tag_subagent_observations(&output.observations, &packet.inputs);
    for obs in &observations {
        let scope = scope_for(obs.scope_hint, &session);
        let hash = content_hash(obs);
        let subagent = obs.entities.iter().any(|e| e.starts_with("subagent:"));
        let norm_key = title_norm_key(&obs.title, &scope, &session.harness, subagent);
        let obs_id = match find_duplicate(conn, packet, obs, &hash, norm_key.as_deref()).await? {
            Some(id) => {
                *counts.get_mut("duplicates").expect("initialised") += 1;
                id
            }
            None => {
                let new = NewObservation {
                    obs,
                    scope: &scope,
                    content_hash: &hash,
                    norm_key: norm_key.as_deref(),
                    event_ids: &event_ids,
                };
                let id = insert_observation(conn, packet, &session, new).await?;
                *counts.get_mut("observations").expect("initialised") += 1;
                first_new = first_new.or(Some(id));
                touched.insert(scope);
                id
            }
        };
        packet_obs.push(obs_id);
        for s in &obs.source_event_seqs {
            obs_by_seq.entry(*s).or_default().push(obs_id);
        }
    }

    for cand in &output.fact_candidates {
        let scope = scope_for(cand.scope_hint, &session);
        let subject_id = resolve_entity(conn, packet.space_id, &cand.subject.kind, &cand.subject.name).await?;
        let cited: Vec<Uuid> =
            cand.evidence_seqs.iter().flat_map(|s| obs_by_seq.get(s).into_iter().flatten()).copied().collect();
        let mut evidence: Vec<Uuid> = Vec::new();
        for id in if cited.is_empty() { &packet_obs } else { &cited } {
            if !evidence.contains(id) {
                evidence.push(*id);
            }
        }
        let active: Option<ActiveFact> = sqlx::query_as(
            "SELECT id, value, value_text, confidence, confirmations, pinned FROM facts
              WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND subject_id = $4
                AND predicate = $5 AND status = 'active'
              FOR UPDATE",
        )
        .bind(packet.space_id)
        .bind(scope.0)
        .bind(&scope.1)
        .bind(subject_id)
        .bind(&cand.predicate)
        .fetch_optional(&mut *conn)
        .await?;
        match active {
            Some(active) if same_value(&active, cand) => {
                confirm(conn, packet, &session, &active, cand, &evidence).await?;
                *counts.get_mut("fact_confirmations").expect("initialised") += 1;
                touched.insert(scope);
            }
            active => {
                let new = NewCandidate {
                    cand,
                    scope: &scope,
                    subject_id,
                    evidence: &evidence,
                    supersedes: active.map(|a| a.id),
                };
                insert_candidate(conn, packet, &session, new).await?;
                *counts.get_mut("fact_candidates").expect("initialised") += 1;
            }
        }
    }

    let hint_scope = scope_for(None, &session);
    for hint in &output.procedure_hints {
        sqlx::query(
            "INSERT INTO procedure_hints (id, space_id, scope_kind, scope_key, task_class, note, observation_id,
                                          session_id, packet_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(new_id())
        .bind(packet.space_id)
        .bind(hint_scope.0)
        .bind(&hint_scope.1)
        .bind(&hint.task_class)
        .bind(&hint.note)
        .bind(first_new)
        .bind(session.id)
        .bind(packet.id)
        .execute(&mut *conn)
        .await?;
        *counts.get_mut("procedure_hints").expect("initialised") += 1;
    }

    if output.needs_escalation {
        let a = Audit {
            action: "packet.needs_escalation",
            table: "work_packets",
            target_id: packet.id,
            reason: Some("runner asked for escalation"),
            ..Default::default()
        };
        audit(conn, packet, a).await?;
    }

    for (kind, key) in &touched {
        notify_invalidate(conn, packet.space_id, *kind, key).await?;
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_observations_citing_subagent_events() {
        let obs = |seqs: Vec<i64>, entities: Vec<&str>| -> ObservedObservation {
            serde_json::from_value(json!({
                "type": "discovery", "title": "t", "facts": [], "concepts": ["how-it-works"], "importance": 0.5,
                "source_event_seqs": seqs, "entities": entities,
            }))
            .unwrap()
        };
        let inputs = json!({"events": [
            {"seq": 1, "kind": "tool_use"},
            {"seq": 2, "kind": "tool_use", "agent_id": "a1", "agent_type": "Explore"},
            {"seq": 3, "kind": "subagent_stop", "agent_id": "a1", "agent_type": "Explore"},
        ]});
        let tagged = tag_subagent_observations(
            &[obs(vec![1], vec!["x"]), obs(vec![2, 3], vec!["x"]), obs(vec![2], vec!["subagent:Explore"])],
            &inputs,
        );
        assert_eq!(tagged[0].entities, vec!["x"]);
        assert_eq!(tagged[1].entities, vec!["x", "subagent:Explore"]);
        assert_eq!(tagged[2].entities, vec!["subagent:Explore"]);
    }

    #[test]
    fn normalizes_titles() {
        assert_eq!(normalize_title("  Fixed: the BUG!! in  `auth.rs` 🚀 "), "fixed the bug in auth rs");
        assert_eq!(normalize_title("🚀 !!"), "");
        let scope = (ScopeKind::Project, "github.com/a/b".to_string());
        let a = title_norm_key("Fixed the bug", &scope, "claude-code", false).unwrap();
        assert_eq!(Some(a.clone()), title_norm_key("fixed: THE bug.", &scope, "claude-code", false));
        assert_eq!(a.len(), 32);
        assert_ne!(Some(a.clone()), title_norm_key("Fixed the bug", &scope, "claude-code", true));
        let other = (ScopeKind::Device, "laptop".to_string());
        assert_ne!(Some(a), title_norm_key("Fixed the bug", &other, "claude-code", false));
        assert_eq!(title_norm_key("--", &scope, "claude-code", false), None);
    }

    #[test]
    fn parses_valid_from() {
        let want = "2026-10-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(parse_valid_from(Some("2026-10-01")), Some(want));
        assert_eq!(parse_valid_from(Some("2026-10-01T00:00:00")), Some(want));
        assert_eq!(parse_valid_from(Some("2026-10-01T02:00:00+02:00")), Some(want));
        assert_eq!(parse_valid_from(Some("soon")), None);
        assert_eq!(parse_valid_from(None), None);
    }
}
