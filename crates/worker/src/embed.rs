//! Embedding jobs (spec 13): load the parts of each queued row, embed them in batches of up to
//! `EMBED_BATCH_SIZE` texts and upsert the vectors with the row's space and scope.

use std::collections::HashSet;
use std::str::FromStr;

use anyhow::Context;
use memvault_core::AppState;
use memvault_core::schemas::common::EmbeddingOwnerTable;
use memvault_core::tei::Embedder;
use pgvector::Vector;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::parts::{Part, parts_for};

/// `jobs(type='embed', payload={"owner_table", "owner_id"})` in the caller's transaction.
/// Lives in core so memvault-ingest (which this crate depends on) can call it too.
pub use memvault_core::jobs::enqueue_embed;

/// What an embed job asks for (`payload` of the jobs row; `model` only on reindex jobs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedTarget {
    pub table: EmbeddingOwnerTable,
    pub id: Uuid,
    pub model: Option<String>,
}

impl EmbedTarget {
    pub fn from_payload(payload: &Value) -> anyhow::Result<Self> {
        let table = payload.get("owner_table").and_then(Value::as_str).context("payload.owner_table missing")?;
        let table = EmbeddingOwnerTable::from_str(table).map_err(|_| anyhow::anyhow!("unknown owner_table {table}"))?;
        let id = payload.get("owner_id").and_then(Value::as_str).context("payload.owner_id missing")?;
        let model = payload.get("model").and_then(Value::as_str).map(str::to_string);
        Ok(Self { table, id: id.parse().context("payload.owner_id is not a uuid")?, model })
    }
}

struct Loaded {
    target: EmbedTarget,
    space_id: Uuid,
    scope_kind: String,
    scope_key: String,
    parts: Vec<Part>,
}

/// The owner row as `(space_id, scope_kind, scope_key, to_jsonb(row))`; entities are global.
async fn load_row(
    conn: &mut PgConnection,
    table: EmbeddingOwnerTable,
    id: Uuid,
) -> Result<Option<(Uuid, String, String, Value)>, sqlx::Error> {
    let scope = match table {
        EmbeddingOwnerTable::Entities => "'global'::text, ''::text",
        _ => "t.scope_kind::text, t.scope_key",
    };
    let sql = format!("SELECT t.space_id, {scope}, to_jsonb(t) FROM {} t WHERE t.id = $1", table.as_str());
    sqlx::query_as(&sql).bind(id).fetch_optional(conn).await
}

/// Embed the given rows. Rows are deduplicated; a row that no longer exists loses its vectors.
/// Texts of all rows go to TEI together, `embed_batch_size` per call. TEI errors fail the whole
/// call (the caller retries the jobs); nothing is written then.
pub async fn embed_targets(state: &AppState, targets: &[EmbedTarget]) -> anyhow::Result<usize> {
    embed_targets_with(state, &*state.tei, targets).await
}

/// [`embed_targets`] with an explicit embedder (the worker uses one with a batch-sized timeout).
pub async fn embed_targets_with(
    state: &AppState,
    tei: &dyn Embedder,
    targets: &[EmbedTarget],
) -> anyhow::Result<usize> {
    let model = tei.model().to_string();
    let max_tokens = state.settings.embed_max_part_tokens;
    let mut conn = state.db.system.acquire().await?;
    let mut loaded = Vec::new();
    let mut gone = Vec::new();
    let mut seen = HashSet::new();
    for t in targets {
        if !seen.insert((t.table, t.id)) {
            continue;
        }
        match load_row(&mut conn, t.table, t.id).await? {
            Some((space_id, scope_kind, scope_key, row)) => {
                let parts = parts_for(t.table, &row, max_tokens);
                loaded.push(Loaded { target: t.clone(), space_id, scope_kind, scope_key, parts });
            }
            None => gone.push(t.clone()),
        }
    }
    drop(conn);

    let texts: Vec<String> = loaded.iter().flat_map(|l| l.parts.iter().map(|p| p.text.clone())).collect();
    let mut vectors = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(state.settings.embed_batch_size.max(1)) {
        let out = tei.embed(chunk).await?;
        anyhow::ensure!(out.len() == chunk.len(), "TEI returned {} vectors for {} texts", out.len(), chunk.len());
        vectors.extend(out);
    }

    let mut tx = state.db.system.begin().await?;
    let mut vectors = vectors.into_iter();
    for l in &loaded {
        let names: Vec<&str> = l.parts.iter().map(|p| p.name.as_str()).collect();
        for part in &l.parts {
            let v = vectors.next().expect("one vector per part");
            sqlx::query(
                "INSERT INTO embeddings (space_id, owner_table, owner_id, part, model, embedding, scope_kind, scope_key)
                 VALUES ($1, $2, $3, $4, $5, $6, $7::text::scope_kind, $8)
                 ON CONFLICT (owner_table, owner_id, part, model) DO UPDATE
                   SET embedding = EXCLUDED.embedding, space_id = EXCLUDED.space_id,
                       scope_kind = EXCLUDED.scope_kind, scope_key = EXCLUDED.scope_key, created_at = now()",
            )
            .bind(l.space_id)
            .bind(l.target.table.as_str())
            .bind(l.target.id)
            .bind(&part.name)
            .bind(&model)
            .bind(Vector::from(v))
            .bind(&l.scope_kind)
            .bind(&l.scope_key)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "DELETE FROM embeddings WHERE owner_table = $1 AND owner_id = $2 AND model = $3 AND part <> ALL($4)",
        )
        .bind(l.target.table.as_str())
        .bind(l.target.id)
        .bind(&model)
        .bind(&names)
        .execute(&mut *tx)
        .await?;
    }
    for t in &gone {
        sqlx::query("DELETE FROM embeddings WHERE owner_table = $1 AND owner_id = $2")
            .bind(t.table.as_str())
            .bind(t.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    metrics::counter!("memsvc_embed_parts_total").increment(texts.len() as u64);
    Ok(texts.len())
}

/// Queue embed jobs for every row lacking a vector for `model` (spec 13 backfill). Rows that
/// already have a queued or running embed job are skipped. Returns the number of jobs queued.
pub async fn reindex(pool: &PgPool, model: &str) -> Result<u64, sqlx::Error> {
    let mut queued = 0;
    for table in EmbeddingOwnerTable::ALL.iter() {
        let sql = format!(
            "INSERT INTO jobs (id, type, payload)
             SELECT gen_random_uuid(), 'embed',
                    jsonb_build_object('owner_table', $1::text, 'owner_id', t.id, 'model', $2::text)
               FROM {} t
              WHERE NOT EXISTS (SELECT 1 FROM embeddings e
                                 WHERE e.owner_table = $1 AND e.owner_id = t.id AND e.model = $2)
                AND NOT EXISTS (SELECT 1 FROM jobs j
                                 WHERE j.type = 'embed' AND j.status IN ('queued', 'running')
                                   AND j.payload->>'owner_table' = $1 AND j.payload->>'owner_id' = t.id::text)",
            table.as_str()
        );
        queued += sqlx::query(&sql).bind(table.as_str()).bind(model).execute(pool).await?.rows_affected();
    }
    Ok(queued)
}
