//! The consolidation planner (spec 12.1): server-side, no model. One pass per space, in one
//! transaction on the system role:
//!
//! 1. refresh `facts.learned_on_client_ids` from evidence (so packets carry current devices);
//! 2. fact clusters -> `consolidate_facts` packets; lone old candidates are auto-activated;
//! 3. decay and archival;
//! 4. day and week summaries -> `consolidate_summary` packets;
//! 5. procedure hints -> `consolidate_procedures` packets;
//! 6. profile re-rendering.
//!
//! Everything written by one `run_planner` call shares a run id: code-only changes are audited with
//! it, packets carry it in their inputs, and appliers audit with it (so a revert undoes the run).

pub mod clusters;
pub mod decay;
mod housekeeping;
mod procedures;
mod summaries;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use memvault_core::ids::new_id;
use memvault_core::schemas::admin::ConsolidateResponse;
use memvault_core::schemas::common::{OutputSchemaName, PacketKind};
use memvault_core::{ApiError, AppState};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::AuditCtx;
use crate::profiles;
use crate::tx::system_tx;

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

/// One planning pass as of `now` over one space, in the caller's transaction.
pub async fn plan_space(
    conn: &mut PgConnection,
    state: &AppState,
    space_id: Uuid,
    run_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Tally, ApiError> {
    let ctx = AuditCtx::planner(space_id, run_id);
    let mut tally = Tally::default();
    tally.action("learned_on_refreshed", housekeeping::refresh_learned_on(conn, space_id).await?);
    clusters::plan(conn, state, &ctx, now, &mut tally).await?;
    let (observations, facts) = decay::decay(conn, space_id, now).await?;
    tally.action("decay_observations", observations);
    tally.action("decay_facts", facts);
    tally.action("obs_archive", decay::archive(conn, &ctx, now).await?);
    summaries::plan(conn, state, &ctx, now, &mut tally).await?;
    procedures::plan(conn, state, &ctx, &mut tally).await?;
    tally.action("profiles_rendered", u64::try_from(profiles::refresh_space(conn, space_id).await?).unwrap_or(0));
    Ok(tally)
}

/// One planning pass (spec 12.1) for one space, or every space when `None`.
/// `memvault planner` and `POST /v1/admin/consolidate` call it.
pub async fn run_planner(state: &AppState, space_id: Option<Uuid>) -> Result<ConsolidateResponse, ApiError> {
    run_planner_at(state, space_id, Utc::now()).await
}

/// [`run_planner`] with an explicit clock (tests, backfills). Each space is planned in its own
/// transaction; a space that fails is logged and counted under `space_errors` when the pass covers
/// every space, and fails the call when one space was asked for.
pub async fn run_planner_at(
    state: &AppState,
    space_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<ConsolidateResponse, ApiError> {
    let run_id = new_id();
    let spaces: Vec<Uuid> = match space_id {
        Some(id) => vec![id],
        None => sqlx::query_scalar("SELECT id FROM spaces ORDER BY created_at, id").fetch_all(&state.db.system).await?,
    };
    let mut total = Tally::default();
    for space in spaces {
        let result = async {
            let mut tx = system_tx(&state.db.system).await?;
            let tally = plan_space(&mut tx, state, space, run_id, now).await?;
            tx.commit().await?;
            Ok::<_, ApiError>(tally)
        }
        .await;
        match result {
            Ok(tally) => total.absorb(tally),
            Err(e) if space_id.is_none() => {
                tracing::error!(%space, error = %e, "planning failed for space");
                total.action("space_errors", 1);
            }
            Err(e) => return Err(e),
        }
    }
    for (name, n) in &total.actions {
        metrics::counter!("memsvc_consolidation_actions_total", "action" => name.clone())
            .increment(u64::try_from(*n).unwrap_or(0));
    }
    tracing::info!(%run_id, packets = ?total.packets, actions = ?total.actions, "consolidation planned");
    Ok(ConsolidateResponse { run_id, packets_created: total.packets, actions: total.actions })
}
