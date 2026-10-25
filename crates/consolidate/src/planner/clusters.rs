//! Fact clusters (spec 12.1 step 1).
//!
//! A candidate is linked to the facts it competes with: the active fact it conflicts with
//! (`supersedes_id`), any active or candidate fact on the same `(scope, subject, predicate)`, and any
//! active or candidate fact whose `value` embedding has cosine > 0.85 to it. Linked rows form
//! clusters (connected components); a cluster of at least two rows becomes a `consolidate_facts`
//! packet. A candidate left alone is auto-activated once it is older than 24 h, has no conflict
//! and `confidence >= 0.6`, audited `fact.create`; otherwise it waits for the next cluster.
//!
//! Similarity uses the stored `value` part embeddings; a candidate that has none yet (the worker
//! embeds asynchronously) is embedded on the fly, and an active fact without one is not found
//! until it is embedded.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use memvault_core::ApiError;
use memvault_core::AppState;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::common::{OutputSchemaName, PacketKind, ScopeKind};
use pgvector::Vector;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use super::scope::ScopeFilter;
use super::{NewPacket, Tally, create_packet, profile_md};
use crate::audit::{AuditCtx, record, snapshot};

/// Cosine similarity above which two fact values count as the same fact.
pub const COSINE_THRESHOLD: f64 = 0.85;
/// Age of a lone candidate before it is auto-activated.
pub const AUTO_ACTIVATE_AFTER_HOURS: i64 = 24;
/// Confidence a lone candidate needs to be auto-activated.
pub const AUTO_ACTIVATE_MIN_CONFIDENCE: f64 = 0.6;
/// Candidates examined per space and pass.
pub const MAX_CANDIDATES: i64 = 500;
/// Rows in one packet; candidates first.
pub const MAX_CLUSTER_ROWS: usize = 25;
/// Evidence observations listed per fact in the packet.
const EVIDENCE_PER_FACT: usize = 5;
/// Packets (done or failed) a cluster may use up before the planner leaves it alone.
pub const MAX_GENERATIONS: i64 = 3;
const NEIGHBORS_PER_CANDIDATE: i64 = 20;

#[derive(Debug, sqlx::FromRow)]
struct Candidate {
    id: Uuid,
    scope_kind: ScopeKind,
    scope_key: String,
    subject_id: Uuid,
    predicate: String,
    value_text: String,
    confidence: f64,
    conflict: bool,
    first_seen_at: DateTime<Utc>,
}

/// Connected components of the graph `nodes`/`edges`, each sorted, ordered by smallest member.
pub fn group(nodes: &[Uuid], edges: &[(Uuid, Uuid)]) -> Vec<Vec<Uuid>> {
    let mut parent: HashMap<Uuid, Uuid> = nodes.iter().map(|n| (*n, *n)).collect();
    fn find(parent: &mut HashMap<Uuid, Uuid>, x: Uuid) -> Uuid {
        let p = parent[&x];
        if p == x {
            return x;
        }
        let root = find(parent, p);
        parent.insert(x, root);
        root
    }
    for (a, b) in edges {
        parent.entry(*a).or_insert(*a);
        parent.entry(*b).or_insert(*b);
        let (ra, rb) = (find(&mut parent, *a), find(&mut parent, *b));
        if ra != rb {
            parent.insert(ra, rb);
        }
    }
    let ids: Vec<Uuid> = parent.keys().copied().collect();
    let mut groups: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for id in ids {
        let root = find(&mut parent, id);
        groups.entry(root).or_default().push(id);
    }
    let mut out: Vec<Vec<Uuid>> = groups.into_values().collect();
    for g in &mut out {
        g.sort();
    }
    out.sort();
    out
}

/// `consolidate_facts:<space>:<hash of the sorted row ids>`.
pub fn idempotency_key(space_id: Uuid, ids: &[Uuid]) -> String {
    let mut sorted = ids.to_vec();
    sorted.sort();
    let digest = Sha256::digest(sorted.iter().map(Uuid::to_string).collect::<Vec<_>>().join(",").as_bytes());
    format!("consolidate_facts:{space_id}:{}", &hex::encode(digest)[..24])
}

/// Earlier packets of the same member set that ended without settling the cluster, counted per
/// idempotency key so a finished packet does not block planning the cluster again.
async fn finished_generations(conn: &mut PgConnection, base_key: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM work_packets
          WHERE status IN ('done', 'failed') AND (idempotency_key = $1 OR idempotency_key LIKE $1 || ':g%')",
    )
    .bind(base_key)
    .fetch_one(conn)
    .await
}

/// The `value` embedding of each candidate: stored, else embedded now (best effort).
async fn candidate_vectors(
    conn: &mut PgConnection,
    state: &AppState,
    candidates: &[Candidate],
) -> Result<HashMap<Uuid, Vector>, sqlx::Error> {
    let ids: Vec<Uuid> = candidates.iter().map(|c| c.id).collect();
    let stored: Vec<(Uuid, Vector)> = sqlx::query_as(
        "SELECT owner_id, embedding FROM embeddings
          WHERE owner_table = 'facts' AND part = 'value' AND model = $1 AND owner_id = ANY($2)",
    )
    .bind(&state.settings.embed_model)
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut vectors: HashMap<Uuid, Vector> = stored.into_iter().collect();
    let missing: Vec<&Candidate> = candidates.iter().filter(|c| !vectors.contains_key(&c.id)).collect();
    if !missing.is_empty() {
        let texts: Vec<String> = missing.iter().map(|c| c.value_text.clone()).collect();
        match state.tei.embed(&texts).await {
            Ok(embedded) => {
                for (c, v) in missing.iter().zip(embedded) {
                    vectors.insert(c.id, Vector::from(v));
                }
            }
            Err(e) => tracing::warn!(error = %e, "embedding unavailable; clustering without cosine links"),
        }
    }
    Ok(vectors)
}

/// Links between candidates and the rows they compete with.
async fn edges(
    conn: &mut PgConnection,
    state: &AppState,
    space_id: Uuid,
    candidates: &[Candidate],
) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
    let ids: Vec<Uuid> = candidates.iter().map(|c| c.id).collect();
    let mut out: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT c.id, c.supersedes_id FROM facts c
          WHERE c.id = ANY($1) AND c.supersedes_id IS NOT NULL
            AND EXISTS (SELECT 1 FROM facts a WHERE a.id = c.supersedes_id AND a.space_id = c.space_id AND a.status = 'active')",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    out.extend(
        sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT c.id, o.id FROM facts c
               JOIN facts o ON o.space_id = c.space_id AND o.scope_kind = c.scope_kind AND o.scope_key = c.scope_key
                           AND o.subject_id = c.subject_id AND o.predicate = c.predicate AND o.id <> c.id
                           AND o.status IN ('active', 'candidate')
              WHERE c.id = ANY($1) AND c.space_id = $2",
        )
        .bind(&ids)
        .bind(space_id)
        .fetch_all(&mut *conn)
        .await?,
    );
    let vectors = candidate_vectors(conn, state, candidates).await?;
    for c in candidates {
        let Some(v) = vectors.get(&c.id) else { continue };
        // Exact scan on purpose: an HNSW search filtered by space could miss neighbours.
        let near: Vec<Uuid> = sqlx::query_scalar(
            "SELECT e.owner_id FROM embeddings e JOIN facts f ON f.id = e.owner_id
              WHERE e.owner_table = 'facts' AND e.part = 'value' AND e.model = $1 AND e.space_id = $2
                AND f.space_id = $2 AND f.status IN ('active', 'candidate') AND f.id <> $3
                AND (e.embedding <=> $4) < $5
              LIMIT $6",
        )
        .bind(&state.settings.embed_model)
        .bind(space_id)
        .bind(c.id)
        .bind(v)
        .bind(1.0 - COSINE_THRESHOLD)
        .bind(NEIGHBORS_PER_CANDIDATE)
        .fetch_all(&mut *conn)
        .await?;
        out.extend(near.into_iter().map(|n| (c.id, n)));
    }
    Ok(out)
}

/// Fact rows of a cluster as packet input, with up to five evidence observations each.
async fn fact_rows(conn: &mut PgConnection, space_id: Uuid, ids: &[Uuid]) -> Result<Vec<Value>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
                  'id', f.id, 'status', f.status, 'scope', f.scope_kind::text || ':' || f.scope_key,
                  'subject', jsonb_build_object('kind', e.kind, 'name', e.canonical_name),
                  'predicate', f.predicate, 'value', f.value, 'value_text', f.value_text,
                  'confidence', f.confidence, 'importance', f.importance, 'conflict', f.conflict,
                  'supersedes_id', f.supersedes_id, 'confirmations', f.confirmations, 'pinned', f.pinned,
                  'valid_from', f.valid_from, 'first_seen_at', f.first_seen_at, 'last_confirmed_at', f.last_confirmed_at,
                  'devices', coalesce((SELECT jsonb_agg(c.device_id ORDER BY c.device_id) FROM clients c
                                        WHERE c.id = ANY (f.learned_on_client_ids)), '[]'::jsonb),
                  'evidence', coalesce((SELECT jsonb_agg(jsonb_build_object('id', o.id, 'type', o.type, 'title', o.title,
                                                                           'created_at', o.created_at)
                                                         ORDER BY o.created_at DESC)
                                          FROM (SELECT * FROM observations ob WHERE ob.id = ANY (f.evidence_observation_ids)
                                                 ORDER BY ob.created_at DESC LIMIT $3) o), '[]'::jsonb))
           FROM facts f JOIN entities e ON e.id = f.subject_id
          WHERE f.space_id = $1 AND f.id = ANY($2)
          ORDER BY (f.status = 'candidate') DESC, f.first_seen_at, f.id",
    )
    .bind(space_id)
    .bind(ids)
    .bind(i64::try_from(EVIDENCE_PER_FACT).unwrap_or(5))
    .fetch_all(conn)
    .await
}

/// Ids of facts already in a pending or leased `consolidate_facts` packet of the space.
async fn in_open_packets(conn: &mut PgConnection, space_id: Uuid) -> Result<HashSet<Uuid>, sqlx::Error> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT jsonb_array_elements_text(inputs -> 'fact_ids') FROM work_packets
          WHERE space_id = $1 AND kind = 'consolidate_facts' AND status IN ('pending', 'leased')
            AND jsonb_typeof(inputs -> 'fact_ids') = 'array'",
    )
    .bind(space_id)
    .fetch_all(conn)
    .await?;
    Ok(ids.iter().filter_map(|s| s.parse().ok()).collect())
}

/// Activate a lone candidate (audited `fact.create`). `false` when an active fact holds its key.
async fn auto_activate(
    conn: &mut PgConnection,
    ctx: &AuditCtx,
    c: &Candidate,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let Some(still_candidate) = sqlx::query_scalar::<_, bool>(
        "SELECT status = 'candidate' FROM facts WHERE id = $1 AND space_id = $2 FOR UPDATE",
    )
    .bind(c.id)
    .bind(ctx.space_id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(false);
    };
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM facts WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3
                           AND subject_id = $4 AND predicate = $5 AND status = 'active')",
    )
    .bind(ctx.space_id)
    .bind(c.scope_kind)
    .bind(&c.scope_key)
    .bind(c.subject_id)
    .bind(&c.predicate)
    .fetch_one(&mut *conn)
    .await?;
    if !still_candidate || taken {
        return Ok(false);
    }
    let before = snapshot(conn, "facts", c.id).await?;
    sqlx::query("UPDATE facts SET status = 'active', valid_from = coalesce(valid_from, $2) WHERE id = $1")
        .bind(c.id)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    let after = snapshot(conn, "facts", c.id).await?;
    record(conn, ctx, "fact.create", "facts", c.id, before, after, Some("lone candidate older than 24h, no conflict"))
        .await?;
    notify_invalidate(conn, ctx.space_id, c.scope_kind, &c.scope_key).await?;
    Ok(true)
}

/// Cluster candidates, create packets, auto-activate lone candidates.
pub async fn plan(
    conn: &mut PgConnection,
    state: &AppState,
    ctx: &AuditCtx,
    now: DateTime<Utc>,
    filter: &ScopeFilter,
    tally: &mut Tally,
) -> Result<(), ApiError> {
    let space_id = ctx.space_id;
    let scope = ScopeFilter::condition("scope_kind", "scope_key", 3);
    let candidates: Vec<Candidate> = sqlx::query_as(&format!(
        "SELECT id, scope_kind, scope_key, subject_id, predicate, value_text, confidence::float8 AS confidence, conflict,
                first_seen_at
           FROM facts WHERE space_id = $1 AND status = 'candidate' AND {scope} ORDER BY first_seen_at, id LIMIT $2",
    ))
    .bind(space_id)
    .bind(MAX_CANDIDATES)
    .bind(filter.only_kind())
    .bind(filter.only_key())
    .bind(filter.skipped())
    .fetch_all(&mut *conn)
    .await?;
    if candidates.is_empty() {
        return Ok(());
    }
    let nodes: Vec<Uuid> = candidates.iter().map(|c| c.id).collect();
    let links = edges(conn, state, space_id, &candidates).await?;
    let candidate_ids: HashSet<Uuid> = nodes.iter().copied().collect();
    let busy = in_open_packets(conn, space_id).await?;
    let mut clustered: HashSet<Uuid> = HashSet::new();

    for members in group(&nodes, &links).into_iter().filter(|g| g.len() >= 2) {
        clustered.extend(members.iter().copied());
        if members.iter().any(|m| busy.contains(m)) {
            continue;
        }
        // candidates first, then the rows they compete with, capped
        let mut ordered: Vec<Uuid> = members.iter().copied().filter(|m| candidate_ids.contains(m)).collect();
        ordered.extend(members.iter().copied().filter(|m| !candidate_ids.contains(m)));
        ordered.truncate(MAX_CLUSTER_ROWS);
        // a cluster whose earlier packets ended done or failed and left candidates behind is planned
        // again under a new generation, up to a cap
        let base_key = idempotency_key(space_id, &ordered);
        let generation = finished_generations(conn, &base_key).await?;
        if generation >= MAX_GENERATIONS {
            tracing::warn!(key = %base_key, "cluster left unsettled by {generation} packets; not planned again");
            continue;
        }
        let key = if generation == 0 { base_key } else { format!("{base_key}:g{generation}") };
        let facts = fact_rows(conn, space_id, &ordered).await?;
        let first = candidates.iter().find(|c| ordered.contains(&c.id)).expect("a cluster has a candidate");
        let inputs = json!({
            "run_id": ctx.run_id,
            "fact_ids": ordered,
            "facts": facts,
            "profile_md": profile_md(conn, space_id, first.scope_kind, &first.scope_key).await?,
        });
        let created = create_packet(
            conn,
            state,
            NewPacket {
                space_id,
                kind: PacketKind::ConsolidateFacts,
                prompt: "consolidate.facts",
                schema: OutputSchemaName::ConsolidateFactsV1,
                inputs,
                idempotency_key: key,
            },
        )
        .await?;
        if created {
            tally.packet(PacketKind::ConsolidateFacts);
        }
    }

    let old_enough = now - Duration::hours(AUTO_ACTIVATE_AFTER_HOURS);
    for c in candidates.iter().filter(|c| !clustered.contains(&c.id)) {
        let eligible = c.first_seen_at <= old_enough && !c.conflict && c.confidence >= AUTO_ACTIVATE_MIN_CONFIDENCE;
        if eligible && auto_activate(conn, ctx, c, now).await? {
            tally.action("auto_activate", 1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn ids(n: usize) -> Vec<Uuid> {
        (0..n).map(|_| Uuid::now_v7()).collect()
    }

    #[test]
    fn components() {
        let v = ids(6);
        let groups = group(&v, &[(v[0], v[1]), (v[1], v[2]), (v[3], v[4])]);
        assert_eq!(groups.len(), 3);
        let sizes: BTreeSet<usize> = groups.iter().map(Vec::len).collect();
        assert_eq!(sizes, BTreeSet::from([1, 2, 3]));
        assert!(groups.iter().any(|g| g.len() == 3 && g.contains(&v[0]) && g.contains(&v[2])));
    }

    #[test]
    fn edges_may_name_unlisted_nodes() {
        let v = ids(3);
        let groups = group(&v[..1], &[(v[0], v[1]), (v[1], v[2])]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 3);
    }

    #[test]
    fn key_ignores_order() {
        let space = Uuid::now_v7();
        let v = ids(3);
        let mut rev = v.clone();
        rev.reverse();
        assert_eq!(idempotency_key(space, &v), idempotency_key(space, &rev));
        assert_ne!(idempotency_key(space, &v), idempotency_key(space, &v[..2]));
    }
}
