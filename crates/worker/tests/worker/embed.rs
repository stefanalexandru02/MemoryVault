use memvault_worker::{embed, embed_pass};
use sqlx::PgPool;

use crate::common::env;

#[sqlx::test(migrations = "../../migrations")]
async fn embeds_parts_with_space_and_scope(pool: PgPool) {
    let e = env(pool).await;
    let id =
        e.observation("Fix the login bug", Some("Token expiry was off by one"), &["uses jwt", "exp in seconds"]).await;
    e.embed_job("observations", id).await;

    assert_eq!(embed_pass(&e.state, "t").await.unwrap(), 1);

    assert_eq!(e.parts(id).await, ["fact:0", "fact:1", "narrative", "title"]);
    let (space, kind, key, model, dims): (uuid::Uuid, String, String, String, i32) = sqlx::query_as(
        "SELECT space_id, scope_kind::text, scope_key, model, vector_dims(embedding) FROM embeddings
          WHERE owner_id = $1 AND part = 'title'",
    )
    .bind(id)
    .fetch_one(&e.pool)
    .await
    .unwrap();
    assert_eq!((space, kind.as_str(), key.as_str()), (e.who.space_id, "project", "proj-a"));
    assert_eq!((model.as_str(), dims), ("bge-m3", 1024));
    assert_eq!(e.job_statuses().await, ["done"]);

    // The stored vector is what the embedder produced for the text.
    let stored: pgvector::Vector =
        sqlx::query_scalar("SELECT embedding FROM embeddings WHERE owner_id = $1 AND part = 'title'")
            .bind(id)
            .fetch_one(&e.pool)
            .await
            .unwrap();
    let expect = e.tei.vector("Fix the login bug");
    assert!(stored.as_slice().iter().zip(&expect).all(|(a, b)| (a - b).abs() < 1e-5));
}

#[sqlx::test(migrations = "../../migrations")]
async fn batches_at_most_64_texts_per_tei_call(pool: PgPool) {
    let e = env(pool).await;
    for i in 0..40 {
        let id = e.observation(&format!("title {i}"), Some(&format!("narrative {i}")), &[]).await;
        e.embed_job("observations", id).await;
    }
    assert_eq!(embed_pass(&e.state, "t").await.unwrap(), 40);
    let calls = e.tei.calls.lock().unwrap().clone();
    let embeds: Vec<usize> = calls.iter().filter(|(m, _)| *m == "embed").map(|(_, n)| *n).collect();
    assert_eq!(embeds, [64, 16]);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM embeddings").fetch_one(&e.pool).await.unwrap();
    assert_eq!(n, 80);
}

#[sqlx::test(migrations = "../../migrations")]
async fn reembedding_drops_removed_parts_and_ignores_duplicate_jobs(pool: PgPool) {
    let e = env(pool).await;
    let id = e.observation("t", Some("n"), &["a", "b", "c"]).await;
    e.embed_job("observations", id).await;
    e.embed_job("observations", id).await;
    embed_pass(&e.state, "t").await.unwrap();
    assert_eq!(e.parts(id).await.len(), 5);
    assert_eq!(e.tei.calls.lock().unwrap().iter().map(|c| c.1).sum::<usize>(), 5);

    sqlx::query("UPDATE observations SET facts = ARRAY['a'], narrative = NULL WHERE id = $1")
        .bind(id)
        .execute(&e.pool)
        .await
        .unwrap();
    e.embed_job("observations", id).await;
    embed_pass(&e.state, "t").await.unwrap();
    assert_eq!(e.parts(id).await, ["fact:0", "title"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn tei_failure_requeues_with_backoff_then_succeeds(pool: PgPool) {
    let e = env(pool).await;
    let id = e.observation("title", None, &[]).await;
    e.embed_job("observations", id).await;

    e.tei.set_down(true);
    embed_pass(&e.state, "t").await.unwrap();
    assert_eq!(e.job_statuses().await, ["queued"]);
    let (attempts, err, delayed): (i32, Option<String>, bool) =
        sqlx::query_as("SELECT attempts, last_error, run_after > now() FROM jobs").fetch_one(&e.pool).await.unwrap();
    assert_eq!(attempts, 1);
    assert!(err.is_some() && delayed);
    assert!(e.parts(id).await.is_empty());
    // Not claimable until the backoff passes.
    assert_eq!(embed_pass(&e.state, "t").await.unwrap(), 0);

    e.tei.set_down(false);
    e.make_due().await;
    assert_eq!(embed_pass(&e.state, "t").await.unwrap(), 1);
    assert_eq!(e.job_statuses().await, ["done"]);
    assert_eq!(e.parts(id).await, ["title"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn bad_payload_fails_permanently_and_deleted_rows_lose_vectors(pool: PgPool) {
    let e = env(pool).await;
    sqlx::query(
        "INSERT INTO jobs (id, type, payload) VALUES (gen_random_uuid(), 'embed', '{\"owner_table\": \"nope\"}')",
    )
    .execute(&e.pool)
    .await
    .unwrap();
    let id = e.observation("title", None, &[]).await;
    e.embed_job("observations", id).await;
    embed_pass(&e.state, "t").await.unwrap();
    assert_eq!(e.parts(id).await, ["title"]);
    let mut s = e.job_statuses().await;
    s.sort();
    assert_eq!(s, ["done", "failed"]);

    sqlx::query("DELETE FROM observations WHERE id = $1").bind(id).execute(&e.pool).await.unwrap();
    e.embed_job("observations", id).await;
    embed_pass(&e.state, "t").await.unwrap();
    assert!(e.parts(id).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn every_owner_table_is_embedded(pool: PgPool) {
    let e = env(pool).await;
    let space = e.who.space_id;
    let entity = memvault_core::ids::new_id();
    sqlx::query("INSERT INTO entities (id, space_id, kind, canonical_name) VALUES ($1, $2, 'tool', 'ripgrep')")
        .bind(entity)
        .bind(space)
        .execute(&e.pool)
        .await
        .unwrap();
    let fact = memvault_core::ids::new_id();
    sqlx::query(
        "INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text)
         VALUES ($1, $2, 'global', '', $3, 'uses.tool', '\"rg\"', 'rg')",
    )
    .bind(fact)
    .bind(space)
    .bind(entity)
    .execute(&e.pool)
    .await
    .unwrap();
    let proc_id = memvault_core::ids::new_id();
    sqlx::query(
        "INSERT INTO procedures (id, space_id, scope_kind, scope_key, name, task_class, trigger_text, body_md)
         VALUES ($1, $2, 'project', 'p', 'n', 'build', 'when building', '1. run cargo')",
    )
    .bind(proc_id)
    .bind(space)
    .execute(&e.pool)
    .await
    .unwrap();
    let summary = memvault_core::ids::new_id();
    sqlx::query(
        "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, request, learned, next_steps, extracted_by)
         VALUES ($1, $2, 'project', 'p', 'session', 'req', 'lrn', 'nxt', '{}')",
    )
    .bind(summary)
    .bind(space)
    .execute(&e.pool)
    .await
    .unwrap();
    for (t, id) in [("entities", entity), ("facts", fact), ("procedures", proc_id), ("summaries", summary)] {
        e.embed_job(t, id).await;
    }
    embed_pass(&e.state, "t").await.unwrap();
    assert_eq!(e.parts(entity).await, ["name"]);
    assert_eq!(e.parts(fact).await, ["value"]);
    assert_eq!(e.parts(proc_id).await, ["body", "trigger"]);
    assert_eq!(e.parts(summary).await, ["learned", "next_steps", "request"]);
    let (kind,): (String,) = sqlx::query_as("SELECT scope_kind::text FROM embeddings WHERE owner_id = $1")
        .bind(entity)
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(kind, "global");
}

#[sqlx::test(migrations = "../../migrations")]
async fn reindex_queues_only_rows_lacking_vectors_once(pool: PgPool) {
    let e = env(pool).await;
    let a = e.observation("a", None, &[]).await;
    let b = e.observation("b", None, &[]).await;
    e.embed_job("observations", a).await;
    embed_pass(&e.state, "t").await.unwrap();

    assert_eq!(embed::reindex(&e.pool, "bge-m3").await.unwrap(), 1);
    assert_eq!(embed::reindex(&e.pool, "bge-m3").await.unwrap(), 0);
    // For another model both rows lack vectors; b's job is already queued.
    assert_eq!(embed::reindex(&e.pool, "other").await.unwrap(), 1);
    embed_pass(&e.state, "t").await.unwrap();
    assert_eq!(e.parts(b).await, ["title"]);
    // The job that asked for another model is rejected by this worker.
    let failed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE status = 'failed'").fetch_one(&e.pool).await.unwrap();
    assert_eq!(failed, 1);
}

/// Delegates to FakeTei but rejects any batch containing the text "poison".
struct PoisonTei(std::sync::Arc<memvault_core::tei::FakeTei>);

#[async_trait::async_trait]
impl memvault_core::tei::Embedder for PoisonTei {
    fn model(&self) -> &str {
        self.0.model()
    }
    fn dims(&self) -> usize {
        self.0.dims()
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, memvault_core::tei::TeiError> {
        if texts.iter().any(|t| t.contains("poison")) {
            return Err(memvault_core::tei::TeiError::Failed("422 rejected".into()));
        }
        self.0.embed(texts).await
    }
    async fn rerank(&self, q: &str, t: &[String]) -> Result<Vec<f32>, memvault_core::tei::TeiError> {
        self.0.rerank(q, t).await
    }
    async fn health(&self) -> memvault_core::tei::TeiHealth {
        self.0.health().await
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn poison_row_only_fails_its_own_job(pool: PgPool) {
    let e = env(pool).await;
    let mut good = Vec::new();
    for i in 0..5 {
        let id = e.observation(&format!("good {i}"), None, &[]).await;
        e.embed_job("observations", id).await;
        good.push(id);
    }
    let bad = e.observation("poison", None, &[]).await;
    e.embed_job("observations", bad).await;
    let tei: std::sync::Arc<dyn memvault_core::tei::Embedder> = std::sync::Arc::new(PoisonTei(e.tei.clone()));

    assert_eq!(memvault_worker::embed_pass_with(&e.state, &tei, "t").await.unwrap(), 6);
    for id in good {
        assert_eq!(e.parts(id).await, ["title"]);
    }
    assert!(e.parts(bad).await.is_empty());
    let mut statuses = e.job_statuses().await;
    statuses.sort();
    assert_eq!(statuses, ["done", "done", "done", "done", "done", "queued"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn backfill_jobs_are_claimed_after_live_ones(pool: PgPool) {
    let e = env(pool).await;
    let old = e.observation("old", None, &[]).await;
    assert_eq!(embed::reindex(&e.pool, "bge-m3").await.unwrap(), 1);
    // a fresh row is queued after the backfill, and still comes out first
    let fresh = e.observation("fresh", None, &[]).await;
    e.embed_job("observations", fresh).await;
    let mut owners = Vec::new();
    for _ in 0..2 {
        let claimed = memvault_worker::queue::claim(&e.pool, "t", &["embed"], 1).await.unwrap();
        owners.push(claimed[0].payload["owner_id"].as_str().unwrap().to_string());
    }
    assert_eq!(owners, [fresh.to_string(), old.to_string()]);
    // backfill ids are time-ordered UUIDv7 like every other id
    let versions: Vec<i32> =
        sqlx::query_scalar("SELECT (get_byte(uuid_send(id), 6) >> 4)::int FROM jobs WHERE priority > 0")
            .fetch_all(&e.pool)
            .await
            .unwrap();
    assert_eq!(versions, [7]);
}
