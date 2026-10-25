//! The consolidation planner (spec 12.1): server-side, no model. One pass per space, in one
//! transaction on the system role:
//!
//! 1. refresh `facts.learned_on_client_ids` from evidence (so packets carry current devices);
//! 2. facts disputed by feedback are retracted;
//! 3. fact clusters -> `consolidate_facts` packets; lone old candidates are auto-activated;
//! 4. decay and archival;
//! 5. day and week summaries -> `consolidate_summary` packets;
//! 6. procedure hints -> `consolidate_procedures` packets;
//! 7. profile re-rendering.
//!
//! Everything written by one `run_planner` call shares a run id: code-only changes are audited with
//! it, packets carry it in their inputs, and appliers audit with it (so a revert undoes the run).
//!
//! A pass covers a whole space (the nightly pass, an operator) or one scope of it (a run queued by a
//! session end or by enough new observations, see [`run_plan_job`]). Each pass records why it ran
//! ([`PlanTrigger`]) and which scopes it planned, in `planner_runs` and `scope_planning`. The nightly
//! pass leaves out scopes planned within `CONSOLIDATE_MIN_INTERVAL_S`.

pub mod clusters;
pub mod decay;
pub mod feedback;
mod housekeeping;
mod procedures;
mod runs;
pub mod scope;
mod summaries;

pub use runs::{JobOutcome, run_plan_job};
pub use scope::ScopeFilter;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use memvault_core::ids::new_id;
use memvault_core::schemas::admin::ConsolidateResponse;
use memvault_core::schemas::common::{OutputSchemaName, PacketKind, PlanTrigger, Scope};
use memvault_core::{ApiError, AppState};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::AuditCtx;
use crate::profiles;
use memvault_core::system_tx;

/// Counters of one pass, keyed like the response maps.
#[derive(Debug, Default)]
pub struct Tally {
    pub packets: BTreeMap<String, i64>,
    pub actions: BTreeMap<String, i64>,
}

impl Tally {
    pub fn packet(&mut self, kind: PacketKind) {
        *self.packets.entry(kind.to_string()).or_insert(0) += 1;
    }

    pub fn action(&mut self, name: &str, n: u64) {
        if n > 0 {
            *self.actions.entry(name.to_string()).or_insert(0) += i64::try_from(n).unwrap_or(i64::MAX);
        }
    }

    fn absorb(&mut self, other: Tally) {
        for (k, v) in other.packets {
            *self.packets.entry(k).or_insert(0) += v;
        }
        for (k, v) in other.actions {
            *self.actions.entry(k).or_insert(0) += v;
        }
    }
}

/// A packet the planner creates.
pub(crate) struct NewPacket<'a> {
    pub space_id: Uuid,
    pub kind: PacketKind,
    /// Prompt name without version, e.g. `consolidate.facts`.
    pub prompt: &'a str,
    pub schema: OutputSchemaName,
    pub inputs: Value,
    pub idempotency_key: String,
}

/// Insert a pending packet; `false` when its idempotency key exists already. The prompt ref and
/// model hint come from the loaded prompt registry (newest version), else `<name>@v1` on opus.
pub(crate) async fn create_packet(
    conn: &mut PgConnection,
    state: &AppState,
    p: NewPacket<'_>,
) -> Result<bool, sqlx::Error> {
    let (prompt_ref, model_hint) = match state.prompts.get(p.prompt, None) {
        Ok(spec) => (spec.prompt_ref(), spec.model_hint.clone()),
        Err(_) => (format!("{}@v1", p.prompt), "opus".to_string()),
    };
    let inserted = sqlx::query(
        "INSERT INTO work_packets (id, space_id, kind, prompt_ref, model_hint, inputs, output_schema, idempotency_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (idempotency_key) DO NOTHING",
    )
    .bind(new_id())
    .bind(p.space_id)
    .bind(p.kind)
    .bind(prompt_ref)
    .bind(model_hint)
    .bind(&p.inputs)
    .bind(p.schema)
    .bind(&p.idempotency_key)
    .execute(conn)
    .await?
    .rows_affected();
    if inserted == 1 {
        metrics::counter!("memsvc_packets_created_total", "kind" => p.kind.as_str()).increment(1);
    }
    Ok(inserted == 1)
}

/// The rendered profile block that applies to `(kind, key)`, or the global one (packet context).
pub(crate) async fn profile_md(
    conn: &mut PgConnection,
    space_id: Uuid,
    kind: memvault_core::schemas::common::ScopeKind,
    key: &str,
) -> Result<String, sqlx::Error> {
    let body: Option<String> = sqlx::query_scalar(
        "SELECT body_md FROM profiles WHERE space_id = $1 AND (scope_kind, scope_key) IN (($2::scope_kind, $3), ('global', ''))
          ORDER BY (scope_kind = 'global') LIMIT 1",
    )
    .bind(space_id)
    .bind(kind)
    .bind(key)
    .fetch_optional(conn)
    .await?;
    Ok(body.unwrap_or_default())
}

/// One planning pass as of `now` over the scopes of `filter` in one space, in the caller's transaction.
pub async fn plan_space(
    conn: &mut PgConnection,
    state: &AppState,
    space_id: Uuid,
    run_id: Uuid,
    now: DateTime<Utc>,
    filter: &ScopeFilter,
) -> Result<Tally, ApiError> {
    let ctx = AuditCtx::planner(space_id, run_id);
    let mut tally = Tally::default();
    tally.action("learned_on_refreshed", housekeeping::refresh_learned_on(conn, space_id, filter).await?);
    tally.action("feedback_retract", feedback::retract_disputed(conn, &ctx, now, filter).await?);
    clusters::plan(conn, state, &ctx, now, filter, &mut tally).await?;
    let (observations, facts) = decay::decay(conn, space_id, now, filter).await?;
    tally.action("decay_observations", observations);
    tally.action("decay_facts", facts);
    tally.action("obs_archive", decay::archive(conn, &ctx, now, filter).await?);
    summaries::plan(conn, state, &ctx, now, filter, &mut tally).await?;
    procedures::plan(conn, state, &ctx, filter, &mut tally).await?;
    let rendered = profiles::refresh_space(conn, space_id, filter).await?;
    tally.action("profiles_rendered", u64::try_from(rendered).unwrap_or(0));
    Ok(tally)
}

/// What a planner call covers and why it runs.
#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// `None`: every space.
    pub space_id: Option<Uuid>,
    /// Restrict the run to one scope of `space_id`; `None`: every scope.
    pub scope: Option<Scope>,
    pub trigger: PlanTrigger,
}

impl PlanOptions {
    /// The scheduled pass: every scope of the spaces, except those planned within the minimum interval.
    pub fn nightly(space_id: Option<Uuid>) -> Self {
        Self { space_id, scope: None, trigger: PlanTrigger::Nightly }
    }

    /// An operator's run: it covers what it is asked to, whatever was planned a moment ago.
    pub fn manual(space_id: Option<Uuid>, scope: Option<Scope>) -> Self {
        Self { space_id, scope, trigger: PlanTrigger::Manual }
    }
}

/// One manual planning pass (spec 12.1) for one space, or every space when `None`.
pub async fn run_planner(state: &AppState, space_id: Option<Uuid>) -> Result<ConsolidateResponse, ApiError> {
    run_planner_for(state, &PlanOptions::manual(space_id, None)).await
}

/// [`run_planner`] with an explicit clock (tests, backfills).
pub async fn run_planner_at(
    state: &AppState,
    space_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<ConsolidateResponse, ApiError> {
    run_planner_with(state, &PlanOptions::manual(space_id, None), now).await
}

/// [`run_planner_with`] as of now.
pub async fn run_planner_for(state: &AppState, options: &PlanOptions) -> Result<ConsolidateResponse, ApiError> {
    run_planner_with(state, options, Utc::now()).await
}

/// One planning pass as `options` describe it, as of `now`. Each space is planned in its own
/// transaction; a space that fails is logged and counted under `space_errors` when the pass covers
/// every space, and fails the call when one space was asked for. `memvault planner` (nightly),
/// `POST /v1/admin/consolidate` (manual) and the worker's `plan` jobs call it.
pub async fn run_planner_with(
    state: &AppState,
    options: &PlanOptions,
    now: DateTime<Utc>,
) -> Result<ConsolidateResponse, ApiError> {
    if options.scope.is_some() && options.space_id.is_none() {
        return Err(ApiError::invalid("a scope needs a space"));
    }
    let run_id = new_id();
    let trigger = options.trigger;
    let spaces: Vec<Uuid> = match options.space_id {
        Some(id) => vec![id],
        None => sqlx::query_scalar("SELECT id FROM spaces ORDER BY created_at, id").fetch_all(&state.db.system).await?,
    };
    let mut total = Tally::default();
    for space in spaces {
        let result = async {
            let mut tx = system_tx(&state.db.system).await?;
            let filter = match &options.scope {
                Some(scope) => ScopeFilter::only(scope.clone()),
                None if trigger == PlanTrigger::Nightly => {
                    let min_interval = chrono::Duration::from_std(state.settings.consolidate_min_interval)
                        .unwrap_or(chrono::Duration::zero());
                    ScopeFilter::skipping(&runs::planned_since(&mut tx, space, now - min_interval).await?)
                }
                None => ScopeFilter::default(),
            };
            let tally = plan_space(&mut tx, state, space, run_id, now, &filter).await?;
            runs::record(&mut tx, space, run_id, trigger, &filter, now, &tally).await?;
            tx.commit().await?;
            Ok::<_, ApiError>(tally)
        }
        .await;
        match result {
            Ok(tally) => {
                metrics::counter!("memsvc_planner_runs_total", "trigger" => trigger.as_str()).increment(1);
                total.absorb(tally);
            }
            Err(e) if options.space_id.is_none() => {
                tracing::error!(%space, error = %e, "planning failed for space");
                total.action("space_errors", 1);
            }
            Err(e) => return Err(e),
        }
    }
    for (name, n) in &total.actions {
        metrics::counter!("memsvc_consolidation_actions_total", "action" => name.clone(), "trigger" => trigger.as_str())
            .increment(u64::try_from(*n).unwrap_or(0));
    }
    tracing::info!(%run_id, trigger = trigger.as_str(), packets = ?total.packets, actions = ?total.actions, "consolidation planned");
    Ok(ConsolidateResponse { run_id, trigger: Some(trigger), packets_created: total.packets, actions: total.actions })
}
