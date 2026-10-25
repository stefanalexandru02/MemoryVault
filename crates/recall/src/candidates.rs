//! Candidate SQL per tier (spec 11.2): keyword leg, vector leg, no-query leg, hydration.
//!
//! Keyword leg: ParadeDB BM25 (`search_text @@@ q`, `paradedb.score`) when `pg_search` is installed,
//! otherwise the generated `search_tsv` with `websearch_to_tsquery('simple', ...)` and `ts_rank_cd`.
//! Query words are OR-ed so a long natural-language query still matches rows sharing only some of
//! them; ranking rewards rows matching more. `task_state.entities` terms get half weight.
//!
//! Vector leg: best part per owner (`DISTINCT ON (owner_id)` ordered by distance) over `embeddings`
//! for the configured model, with the recency windows of 11.2 (observations 90 d, summaries 180 d)
//! when `windows` is set (recall), none for search.
//!
//! Every query runs on the caller's `user_tx` connection, so RLS bounds what is visible.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use memvault_core::schemas::common::{
    ContentType, FactStatus, MemoryTable, Scope, ScopeKind, Sensitivity, SummaryGrain,
};
use memvault_core::schemas::memories::{fact_caution, source_label};
use pgvector::Vector;
use serde_json::Value;
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

pub const KEYWORD_LIMIT: i64 = 40;
pub const VECTOR_LIMIT: i64 = 60;
/// Parts fetched per wanted owner before `DISTINCT ON`.
pub const VECTOR_PART_FANOUT: i64 = 4;

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "has", "have", "how", "i", "in", "is", "it", "its",
    "of", "on", "or", "that", "the", "this", "to", "was", "were", "what", "when", "where", "which", "who", "why",
    "will", "with", "after", "before", "into", "about", "over", "then", "than", "there", "these", "those", "we", "you",
    "my", "our", "not", "do", "does", "did", "can",
];

/// Lowercased distinct words of a query in order, without stopwords and one-character tokens.
pub fn query_terms(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let w = w.to_lowercase();
        if w.chars().count() > 1 && !STOPWORDS.contains(&w.as_str()) && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// Per-tier SQL fragments.
struct TierSpec {
    sql_table: &'static str,
    /// SQL expression matched by the `types` filter (and shown as `IndexRow.type`).
    type_expr: &'static str,
    time_col: &'static str,
    has_sensitivity: bool,
    /// Rows that may be shown at all.
    live: &'static str,
    /// Recall recency window on the vector leg.
    vector_window_days: Option<i32>,
    /// ORDER BY of the no-query leg.
    recent_order: &'static str,
}

fn spec(table: MemoryTable) -> TierSpec {
    match table {
        MemoryTable::Observations => TierSpec {
            sql_table: "observations",
            type_expr: "t.type",
            time_col: "created_at",
            has_sensitivity: true,
            live: "t.archived_at IS NULL AND t.superseded_by IS NULL",
            vector_window_days: Some(90),
            recent_order: "t.created_at DESC, t.id DESC",
        },
        MemoryTable::Summaries => TierSpec {
            sql_table: "summaries",
            type_expr: "t.grain",
            time_col: "created_at",
            has_sensitivity: false,
            live: "true",
            vector_window_days: Some(180),
            recent_order: "t.created_at DESC, t.id DESC",
        },
        MemoryTable::Facts => TierSpec {
            sql_table: "facts",
            type_expr: "t.predicate",
            time_col: "first_seen_at",
            has_sensitivity: true,
            live: "t.status = 'active' AND (t.valid_to IS NULL OR t.valid_to > now())",
            vector_window_days: None,
            recent_order: "t.pinned DESC, t.importance * t.confidence DESC, t.last_confirmed_at DESC, t.id DESC",
        },
        MemoryTable::Procedures => TierSpec {
            sql_table: "procedures",
            type_expr: "t.task_class",
            time_col: "created_at",
            has_sensitivity: false,
            live: "t.status = 'active'",
            vector_window_days: None,
            recent_order: "t.last_used_at DESC NULLS LAST, t.success_count DESC, t.created_at DESC, t.id DESC",
        },
        MemoryTable::Documents => TierSpec {
            sql_table: "documents",
            type_expr: "'document'",
            time_col: "created_at",
            has_sensitivity: false,
            live: "t.retired_at IS NULL",
            vector_window_days: None,
            recent_order: "t.created_at DESC, t.source_uri, t.chunk_index, t.id",
        },
    }
}

/// Row filters shared by every leg.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub spaces: Vec<Uuid>,
    /// `None` = every scope in the spaces.
    pub scopes: Option<Vec<Scope>>,
    pub exclude_sensitivity: Vec<Sensitivity>,
    pub types: Option<Vec<String>>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub files: Option<Vec<String>>,
    pub summary_grains: Option<Vec<SummaryGrain>>,
    /// The SessionStart index hides subagent observations (8.3).
    pub exclude_subagent: bool,
    /// Facts: pinned only.
    pub pinned_only: bool,
    /// Point in time: rows valid then replace the live rows (see [`push_live`]).
    pub as_of: Option<DateTime<Utc>>,
}

/// Retirement time of a procedure: the audited `proc.retire`, else when its successor was created.
const PROCEDURE_ENDED_AT: &str = "coalesce(
    (SELECT max(a.created_at) FROM memory_audit a
      WHERE a.target_table = 'procedures' AND a.target_id = t.id AND a.action = 'proc.retire'),
    (SELECT min(n.created_at) FROM procedures n WHERE n.supersedes_id = t.id))";

/// Rows that may be shown: the live ones, or with `as_of` the ones that were valid at that instant.
/// A fact counts from `valid_from` (else `first_seen_at`) until `valid_to`; candidates and merged
/// duplicates (superseded without an end) never held. A procedure counts from `created_at` until
/// it was retired. Observations and summaries count from `created_at`, observations until archived,
/// document chunks until retired.
fn push_live(qb: &mut QueryBuilder<'_, Postgres>, table: MemoryTable, as_of: Option<DateTime<Utc>>) {
    let Some(at) = as_of else {
        qb.push(spec(table).live);
        return;
    };
    match table {
        MemoryTable::Observations => {
            qb.push("t.created_at <= ").push_bind(at).push(" AND (t.archived_at IS NULL OR t.archived_at > ");
            qb.push_bind(at).push(")");
        }
        MemoryTable::Summaries => {
            qb.push("t.created_at <= ").push_bind(at);
        }
        MemoryTable::Facts => {
            qb.push(
                "t.status <> 'candidate' AND NOT (t.status = 'superseded' AND t.valid_to IS NULL) \
                 AND coalesce(t.valid_from, t.first_seen_at) <= ",
            )
            .push_bind(at)
            .push(" AND (t.valid_to IS NULL OR t.valid_to > ")
            .push_bind(at)
            .push(")");
        }
        MemoryTable::Procedures => {
            qb.push("t.created_at <= ")
                .push_bind(at)
                .push(format_args!(" AND (t.status = 'active' OR {PROCEDURE_ENDED_AT} > "))
                .push_bind(at)
                .push(")");
        }
        MemoryTable::Documents => {
            qb.push("t.created_at <= ").push_bind(at).push(" AND (t.retired_at IS NULL OR t.retired_at > ");
            qb.push_bind(at).push(")");
        }
    }
}

fn push_where(qb: &mut QueryBuilder<'_, Postgres>, table: MemoryTable, f: &Filters) {
    let s = spec(table);
    qb.push("t.space_id = ANY(").push_bind(f.spaces.clone()).push(") AND ");
    push_live(qb, table, f.as_of);
    if let Some(scopes) = &f.scopes {
        let kinds: Vec<ScopeKind> = scopes.iter().map(|s| s.kind).collect();
        let keys: Vec<String> = scopes.iter().map(|s| s.key.clone()).collect();
        qb.push(" AND (t.scope_kind, t.scope_key) IN (SELECT * FROM unnest(")
            .push_bind(kinds)
            .push("::scope_kind[], ")
            .push_bind(keys)
            .push("::text[]))");
    }
    if s.has_sensitivity && !f.exclude_sensitivity.is_empty() {
        qb.push(" AND t.sensitivity <> ALL(").push_bind(f.exclude_sensitivity.clone()).push("::text[])");
    }
    if let Some(types) = f.types.as_ref().filter(|t| !t.is_empty()) {
        qb.push(format_args!(" AND {}::text = ANY(", s.type_expr)).push_bind(types.clone()).push("::text[])");
    }
    if let Some(since) = f.since {
        qb.push(format_args!(" AND t.{} >= ", s.time_col)).push_bind(since);
    }
    if let Some(until) = f.until {
        qb.push(format_args!(" AND t.{} < ", s.time_col)).push_bind(until);
    }
    if let Some(files) = f.files.as_ref().filter(|f| !f.is_empty()) {
        if table == MemoryTable::Observations {
            qb.push(" AND t.files && ").push_bind(files.clone()).push("::text[]");
        } else {
            qb.push(" AND false");
        }
    }
    if let Some(grains) = f.summary_grains.as_ref().filter(|_| table == MemoryTable::Summaries) {
        qb.push(" AND t.grain = ANY(").push_bind(grains.clone()).push("::text[])");
    }
    if f.exclude_subagent && table == MemoryTable::Observations {
        qb.push(" AND NOT EXISTS (SELECT 1 FROM unnest(t.entities) AS en(x) WHERE en.x LIKE 'subagent:%')");
    }
    if f.pinned_only && table == MemoryTable::Facts {
        qb.push(" AND t.pinned");
    }
}

/// Ranked `(id, score)` from BM25 (`pg_search`) or the tsvector fallback.
pub async fn keyword_leg(
    conn: &mut PgConnection,
    pg_search: bool,
    table: MemoryTable,
    f: &Filters,
    terms: &[String],
    entity_terms: &[String],
    limit: i64,
) -> Result<Vec<(Uuid, f64)>, sqlx::Error> {
    let mut main: Vec<String> = terms.to_vec();
    let mut extra: Vec<String> = entity_terms.iter().filter(|e| !terms.contains(e)).cloned().collect();
    if main.is_empty() {
        main = std::mem::take(&mut extra);
    }
    if main.is_empty() {
        return Ok(Vec::new());
    }
    let s = spec(table);
    let mut qb = QueryBuilder::<Postgres>::new("");
    if pg_search {
        let q: Vec<String> = main.iter().cloned().chain(extra.iter().map(|e| format!("{e}^0.5"))).collect();
        qb.push(format_args!("SELECT t.id, paradedb.score(t.id)::float8 AS s FROM {} t WHERE ", s.sql_table));
        push_where(&mut qb, table, f);
        qb.push(" AND t.search_text @@@ ").push_bind(q.join(" OR "));
    } else {
        qb.push("SELECT t.id, (ts_rank_cd(t.search_tsv, q.m)");
        if !extra.is_empty() {
            qb.push(" + 0.5 * ts_rank_cd(t.search_tsv, q.e)");
        }
        qb.push(format_args!(")::float8 AS s FROM {} t, (SELECT websearch_to_tsquery('simple', ", s.sql_table))
            .push_bind(main.join(" or "))
            .push(") AS m");
        if !extra.is_empty() {
            qb.push(", websearch_to_tsquery('simple', ").push_bind(extra.join(" or ")).push(") AS e");
        }
        qb.push(") q WHERE ");
        push_where(&mut qb, table, f);
        qb.push(if extra.is_empty() { " AND t.search_tsv @@ q.m" } else { " AND t.search_tsv @@ (q.m || q.e)" });
    }
    qb.push(" ORDER BY s DESC, t.id LIMIT ").push_bind(limit);
    qb.build_query_as::<(Uuid, f64)>().fetch_all(conn).await
}

/// Ranked `(id, cosine)`, best part per owner.
pub async fn vector_leg(
    conn: &mut PgConnection,
    table: MemoryTable,
    f: &Filters,
    qvec: &[f32],
    model: &str,
    windows: bool,
    limit: i64,
) -> Result<Vec<(Uuid, f64)>, sqlx::Error> {
    let s = spec(table);
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT id, (1 - d)::float8 AS s FROM (SELECT DISTINCT ON (pp.id) pp.id, pp.d FROM (\
         SELECT e.owner_id AS id, e.embedding <=> ",
    );
    qb.push_bind(Vector::from(qvec.to_vec()))
        .push(format_args!(
            " AS d FROM embeddings e JOIN {} t ON t.id = e.owner_id WHERE e.owner_table = ",
            s.sql_table
        ))
        .push_bind(s.sql_table)
        .push(" AND e.model = ")
        .push_bind(model.to_string())
        .push(" AND e.space_id = ANY(")
        .push_bind(f.spaces.clone())
        .push(") AND ");
    push_where(&mut qb, table, f);
    if let Some(days) = s.vector_window_days.filter(|_| windows) {
        qb.push(format_args!(" AND t.{} > ", s.time_col));
        match f.as_of {
            Some(at) => qb.push_bind(at),
            None => qb.push("now()"),
        };
        qb.push(" - make_interval(days => ").push_bind(days).push(")");
    }
    qb.push(" ORDER BY d LIMIT ")
        .push_bind(limit * VECTOR_PART_FANOUT)
        .push(") pp ORDER BY pp.id, pp.d) best ORDER BY d, id LIMIT ")
        .push_bind(limit);
    qb.build_query_as::<(Uuid, f64)>().fetch_all(conn).await
}

/// No-query candidates in the tier's natural order (newest / most used first).
pub async fn recent_leg(
    conn: &mut PgConnection,
    table: MemoryTable,
    f: &Filters,
    limit: i64,
    offset: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let s = spec(table);
    let mut qb = QueryBuilder::<Postgres>::new(format!("SELECT t.id FROM {} t WHERE ", s.sql_table));
    push_where(&mut qb, table, f);
    qb.push(format_args!(" ORDER BY {} LIMIT ", s.recent_order)).push_bind(limit).push(" OFFSET ").push_bind(offset);
    qb.build_query_scalar::<Uuid>().fetch_all(conn).await
}

// ---------- hydration ----------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ObsRow {
    pub id: Uuid,
    pub space_id: Uuid,
    pub scope_kind: ScopeKind,
    pub scope_key: String,
    pub session_id: Option<Uuid>,
    pub window_no: Option<i32>,
    #[sqlx(rename = "type")]
    pub obs_type: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub narrative: Option<String>,
    pub facts: Vec<String>,
    pub concepts: Vec<String>,
    pub entities: Vec<String>,
    pub files: Vec<String>,
    pub importance: f32,
    pub sensitivity: Sensitivity,
    pub source_event_ids: Vec<Uuid>,
    pub extracted_by: Value,
    pub written_by_client_id: Option<Uuid>,
    pub token_estimate: i32,
    pub created_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
    pub superseded_by: Option<Uuid>,
    /// Device of the session's client (or of the explicit writer).
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SumRow {
    pub id: Uuid,
    pub space_id: Uuid,
    pub scope_kind: ScopeKind,
    pub scope_key: String,
    pub grain: SummaryGrain,
    pub session_id: Option<Uuid>,
    pub prompt_no: Option<i32>,
    pub period_start: Option<DateTime<Utc>>,
    pub period_end: Option<DateTime<Utc>>,
    pub request: Option<String>,
    pub investigated: Option<String>,
    pub learned: Option<String>,
    pub completed: Option<String>,
    pub next_steps: Option<String>,
    pub notes: Option<String>,
    pub extracted_by: Value,
    pub token_estimate: i32,
    pub created_at: DateTime<Utc>,
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FactRow {
    pub id: Uuid,
    pub space_id: Uuid,
    pub scope_kind: ScopeKind,
    pub scope_key: String,
    pub subject_id: Uuid,
    pub subject_name: String,
    pub subject_kind: String,
    pub predicate: String,
    pub value: Value,
    pub value_text: String,
    pub confidence: f32,
    pub importance: f32,
    pub status: FactStatus,
    pub conflict: bool,
    pub supersedes_id: Option<Uuid>,
    pub superseded_by: Option<Uuid>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub first_seen_at: DateTime<Utc>,
    pub last_confirmed_at: DateTime<Utc>,
    pub confirmations: i32,
    pub evidence_observation_ids: Vec<Uuid>,
    pub learned_on_client_ids: Vec<Uuid>,
    pub access_count: i32,
    pub last_accessed_at: Option<DateTime<Utc>>,
    pub pinned: bool,
    pub sensitivity: Sensitivity,
    pub written_by_client_id: Option<Uuid>,
    pub extracted_by: Option<Value>,
    /// Set while the fact waits for the planner to act on negative feedback.
    pub disputed_at: Option<DateTime<Utc>>,
    /// Device ids the fact was learned on (sorted).
    pub devices: Vec<String>,
}

impl FactRow {
    /// Why to treat the fact with care at `now`, if so.
    pub fn caution(&self, now: DateTime<Utc>) -> Option<String> {
        fact_caution(f64::from(self.confidence), self.last_confirmed_at, self.disputed_at.is_some(), self.pinned, now)
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProcRow {
    pub id: Uuid,
    pub space_id: Uuid,
    pub scope_kind: ScopeKind,
    pub scope_key: String,
    pub name: String,
    pub task_class: String,
    pub trigger_text: String,
    pub body_md: String,
    pub version: i32,
    pub status: String,
    pub supersedes_id: Option<Uuid>,
    pub success_count: i32,
    pub failure_count: i32,
    pub last_used_at: Option<DateTime<Utc>>,
    pub source_observation_ids: Vec<Uuid>,
    pub token_estimate: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DocRow {
    pub id: Uuid,
    pub space_id: Uuid,
    pub scope_kind: ScopeKind,
    pub scope_key: String,
    pub source_uri: String,
    pub title: String,
    pub content_type: ContentType,
    pub chunk_index: i32,
    pub heading_path: Vec<String>,
    pub content: String,
    pub content_hash: String,
    pub token_estimate: i32,
    pub ingested_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
}

impl DocRow {
    /// `uri#Heading > Subheading`.
    pub fn source(&self) -> String {
        source_label(&self.source_uri, &self.heading_path)
    }
}

/// A hydrated row of any tier.
#[derive(Debug, Clone)]
pub enum TierRow {
    Obs(ObsRow),
    Sum(SumRow),
    Fact(FactRow),
    Proc(ProcRow),
    Doc(DocRow),
}

impl TierRow {
    pub fn id(&self) -> Uuid {
        match self {
            TierRow::Obs(r) => r.id,
            TierRow::Sum(r) => r.id,
            TierRow::Fact(r) => r.id,
            TierRow::Proc(r) => r.id,
            TierRow::Doc(r) => r.id,
        }
    }

    pub fn space_id(&self) -> Uuid {
        match self {
            TierRow::Obs(r) => r.space_id,
            TierRow::Sum(r) => r.space_id,
            TierRow::Fact(r) => r.space_id,
            TierRow::Proc(r) => r.space_id,
            TierRow::Doc(r) => r.space_id,
        }
    }

    pub fn scope(&self) -> Scope {
        let (kind, key) = match self {
            TierRow::Obs(r) => (r.scope_kind, &r.scope_key),
            TierRow::Sum(r) => (r.scope_kind, &r.scope_key),
            TierRow::Fact(r) => (r.scope_kind, &r.scope_key),
            TierRow::Proc(r) => (r.scope_kind, &r.scope_key),
            TierRow::Doc(r) => (r.scope_kind, &r.scope_key),
        };
        Scope::new(kind, key.clone())
    }
}

const OBS_SELECT: &str =
    "SELECT t.id, t.space_id, t.scope_kind, t.scope_key, t.session_id, t.window_no, t.type, t.title,
        t.subtitle, t.narrative, t.facts, t.concepts, t.entities, t.files, t.importance, t.sensitivity,
        t.source_event_ids, t.extracted_by, t.written_by_client_id, t.token_estimate, t.created_at, t.archived_at,
        t.superseded_by, c.device_id
   FROM observations t
   LEFT JOIN sessions s ON s.id = t.session_id
   LEFT JOIN clients c ON c.id = coalesce(s.client_id, t.written_by_client_id)
  WHERE t.id = ANY($1)";

const SUM_SELECT: &str = "SELECT t.id, t.space_id, t.scope_kind, t.scope_key, t.grain, t.session_id, t.prompt_no,
        t.period_start, t.period_end, t.request, t.investigated, t.learned, t.completed, t.next_steps, t.notes,
        t.extracted_by, t.token_estimate, t.created_at, c.device_id
   FROM summaries t
   LEFT JOIN sessions s ON s.id = t.session_id
   LEFT JOIN clients c ON c.id = s.client_id
  WHERE t.id = ANY($1)";

const FACT_SELECT: &str = "SELECT t.id, t.space_id, t.scope_kind, t.scope_key, t.subject_id,
        en.canonical_name AS subject_name, en.kind AS subject_kind, t.predicate, t.value, t.value_text, t.confidence,
        t.importance, t.status, t.conflict, t.supersedes_id, t.superseded_by, t.valid_from, t.valid_to,
        t.first_seen_at, t.last_confirmed_at, t.confirmations, t.evidence_observation_ids, t.learned_on_client_ids,
        t.access_count, t.last_accessed_at, t.pinned, t.sensitivity, t.written_by_client_id, t.extracted_by,
        t.disputed_at,
        ARRAY(SELECT DISTINCT c.device_id FROM clients c
               WHERE c.id = ANY(CASE WHEN cardinality(t.learned_on_client_ids) > 0 THEN t.learned_on_client_ids
                                     ELSE ARRAY[t.written_by_client_id] END)
               ORDER BY 1) AS devices
   FROM facts t JOIN entities en ON en.id = t.subject_id
  WHERE t.id = ANY($1)";

const PROC_SELECT: &str = "SELECT t.id, t.space_id, t.scope_kind, t.scope_key, t.name, t.task_class, t.trigger_text,
        t.body_md, t.version, t.status, t.supersedes_id, t.success_count, t.failure_count, t.last_used_at,
        t.source_observation_ids, t.token_estimate, t.created_at
   FROM procedures t
  WHERE t.id = ANY($1)";

const DOC_SELECT: &str = "SELECT t.id, t.space_id, t.scope_kind, t.scope_key, t.source_uri, t.title, t.content_type,
        t.chunk_index, t.heading_path, t.content, t.content_hash, t.token_estimate, t.ingested_by, t.created_at,
        t.retired_at
   FROM documents t
  WHERE t.id = ANY($1)";

/// Full rows of `table` for `ids` (whatever RLS lets the caller see), keyed by id.
pub async fn hydrate(
    conn: &mut PgConnection,
    table: MemoryTable,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, TierRow>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ids = ids.to_vec();
    let rows: Vec<TierRow> = match table {
        MemoryTable::Observations => sqlx::query_as::<_, ObsRow>(OBS_SELECT)
            .bind(ids)
            .fetch_all(conn)
            .await?
            .into_iter()
            .map(TierRow::Obs)
            .collect(),
        MemoryTable::Summaries => sqlx::query_as::<_, SumRow>(SUM_SELECT)
            .bind(ids)
            .fetch_all(conn)
            .await?
            .into_iter()
            .map(TierRow::Sum)
            .collect(),
        MemoryTable::Facts => sqlx::query_as::<_, FactRow>(FACT_SELECT)
            .bind(ids)
            .fetch_all(conn)
            .await?
            .into_iter()
            .map(TierRow::Fact)
            .collect(),
        MemoryTable::Procedures => sqlx::query_as::<_, ProcRow>(PROC_SELECT)
            .bind(ids)
            .fetch_all(conn)
            .await?
            .into_iter()
            .map(TierRow::Proc)
            .collect(),
        MemoryTable::Documents => sqlx::query_as::<_, DocRow>(DOC_SELECT)
            .bind(ids)
            .fetch_all(conn)
            .await?
            .into_iter()
            .map(TierRow::Doc)
            .collect(),
    };
    Ok(rows.into_iter().map(|r| (r.id(), r)).collect())
}

/// One embedding per `(owner_table, owner_id)`: the part closest to the query, or the first part.
pub async fn best_part_embeddings(
    conn: &mut PgConnection,
    keys: &[(MemoryTable, Uuid)],
    model: &str,
    qvec: Option<&[f32]>,
) -> Result<HashMap<(MemoryTable, Uuid), Vec<f32>>, sqlx::Error> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let tables: Vec<MemoryTable> = keys.iter().map(|k| k.0).collect();
    let ids: Vec<Uuid> = keys.iter().map(|k| k.1).collect();
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT DISTINCT ON (e.owner_table, e.owner_id) e.owner_table, e.owner_id, e.embedding FROM embeddings e \
         WHERE e.model = ",
    );
    qb.push_bind(model.to_string())
        .push(" AND (e.owner_table, e.owner_id) IN (SELECT * FROM unnest(")
        .push_bind(tables)
        .push("::text[], ")
        .push_bind(ids)
        .push("::uuid[])) ORDER BY e.owner_table, e.owner_id, ");
    match qvec {
        Some(v) => qb.push("e.embedding <=> ").push_bind(Vector::from(v.to_vec())),
        None => qb.push("e.part"),
    };
    let rows: Vec<(MemoryTable, Uuid, Vector)> = qb.build_query_as().fetch_all(conn).await?;
    Ok(rows.into_iter().map(|(t, id, v)| ((t, id), v.to_vec())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_drop_stopwords_and_duplicates() {
        assert_eq!(
            query_terms("Redeploy the memory API after the migration, redeploy!"),
            vec!["redeploy", "memory", "api", "migration"]
        );
        assert!(query_terms("a of the").is_empty());
    }
}
