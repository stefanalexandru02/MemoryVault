use std::time::Duration;

use memvault_worker::{queue, run_worker, sweep};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::common::env;

#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_claims_never_overlap(pool: PgPool) {
    let e = env(pool).await;
    for _ in 0..20 {
        let id = e.observation("t", None, &[]).await;
        e.embed_job("observations", id).await;
    }
    let (a, b) = tokio::join!(queue::claim(&e.pool, "a", &["embed"], 12), queue::claim(&e.pool, "b", &["embed"], 12));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.len() + b.len(), 20);
    assert!(a.iter().all(|x| b.iter().all(|y| x.id != y.id)));
    assert!(a.iter().all(|j| j.attempts == 1));
}

#[sqlx::test(migrations = "../../migrations")]
async fn expired_locks_are_released(pool: PgPool) {
    let e = env(pool).await;
    let id = e.observation("t", None, &[]).await;
    e.embed_job("observations", id).await;
    queue::claim(&e.pool, "dead", &["embed"], 1).await.unwrap();
    assert_eq!(queue::expire_locks(&e.pool).await.unwrap(), 0);
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 minute'").execute(&e.pool).await.unwrap();
    assert_eq!(queue::expire_locks(&e.pool).await.unwrap(), 1);
    assert_eq!(e.job_statuses().await, ["queued"]);
    let again = queue::claim(&e.pool, "alive", &["embed"], 1).await.unwrap();
    assert_eq!(again[0].attempts, 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn retention_is_scheduled_once_per_day(pool: PgPool) {
    let e = env(pool).await;
    assert!(queue::enqueue_daily_retention(&e.pool).await.unwrap());
    assert!(!queue::enqueue_daily_retention(&e.pool).await.unwrap());
}

#[sqlx::test(migrations = "../../migrations")]
async fn sweep_runs_on_an_empty_database(pool: PgPool) {
    let e = env(pool).await;
    sweep::sweep(&e.state).await;
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE type = 'retention'").fetch_one(&e.pool).await.unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn worker_loop_drains_jobs_and_shuts_down(pool: PgPool) {
    let e = env(pool).await;
    let id = e.observation("title", Some("narrative"), &[]).await;
    e.embed_job("observations", id).await;
    // A reindex job and a render_context job for a user that does not exist (nothing to render).
    sqlx::query("INSERT INTO jobs (id, type, payload) VALUES (gen_random_uuid(), 'reindex', '{}')")
        .execute(&e.pool)
        .await
        .unwrap();
    let payload = serde_json::json!({"cache_key": "k", "user_id": uuid::Uuid::nil(), "client_id": uuid::Uuid::nil()});
    sqlx::query("INSERT INTO jobs (id, type, payload) VALUES (gen_random_uuid(), 'render_context', $1)")
        .bind(payload)
        .execute(&e.pool)
        .await
        .unwrap();

    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(run_worker(e.state.clone(), shutdown.clone()));
    let mut done = false;
    for _ in 0..100 {
        let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE status IN ('queued', 'running')")
            .fetch_one(&e.pool)
            .await
            .unwrap();
        if pending == 0 {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(done, "jobs not drained: {:?}", e.job_statuses().await);
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle).await.expect("worker stops").unwrap().unwrap();
    assert_eq!(e.parts(id).await, ["narrative", "title"]);
    assert!(!e.job_statuses().await.contains(&"failed".to_string()));
}

#[sqlx::test(migrations = "../../migrations")]
async fn heartbeat_extends_the_lock_of_running_jobs(pool: PgPool) {
    let e = env(pool).await;
    let id = e.observation("t", None, &[]).await;
    e.embed_job("observations", id).await;
    let jobs = queue::claim(&e.pool, "w", &["embed"], 1).await.unwrap();
    sqlx::query("UPDATE jobs SET locked_until = now() + interval '1 second'").execute(&e.pool).await.unwrap();
    queue::extend_locks(&e.pool, &[jobs[0].id]).await.unwrap();
    let long: bool = sqlx::query_scalar("SELECT locked_until > now() + interval '200 seconds' FROM jobs")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert!(long);
}

#[sqlx::test(migrations = "../../migrations")]
async fn bulk_jobs_do_not_block_render_lane(pool: PgPool) {
    let e = env(pool).await;
    sqlx::query("INSERT INTO jobs (id, type, payload) VALUES (gen_random_uuid(), 'retention', '{}')")
        .execute(&e.pool)
        .await
        .unwrap();
    // The render lane never claims bulk work.
    assert_eq!(memvault_worker::job_pass(&e.state, "r", &["render_context"]).await.unwrap(), 0);
    assert_eq!(e.job_statuses().await, ["queued"]);
    assert_eq!(memvault_worker::job_pass(&e.state, "b", &["reindex", "retention"]).await.unwrap(), 1);
    assert_eq!(e.job_statuses().await, ["done"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn serves_worker_metrics(pool: PgPool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let e = env(pool).await;
    memvault_core::metrics::prometheus_handle();
    sweep::update_gauges(&e.pool).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(memvault_worker::serve_metrics(listener, shutdown.clone()));
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).await.unwrap();
    assert!(body.contains("memsvc_oldest_pending_packet_seconds"), "{body}");
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
}
