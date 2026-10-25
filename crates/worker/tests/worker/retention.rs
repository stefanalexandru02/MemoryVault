use memvault_core::ids::new_id;
use memvault_worker::retention::run_retention;
use sqlx::PgPool;

use crate::common::env;

#[sqlx::test(migrations = "../../migrations")]
async fn trims_old_events_and_deletes_old_rows(pool: PgPool) {
    let e = env(pool).await;
    let s = e.who.space_id;
    let session = new_id();
    sqlx::query(
        "INSERT INTO sessions (id, space_id, client_id, harness, external_session_id) VALUES ($1, $2, $3, 'claude-code', 'x')",
    )
    .bind(session)
    .bind(s)
    .bind(e.who.client_id)
    .execute(&e.pool)
    .await
    .unwrap();
    let big = "x".repeat(2000);
    for (seq, age) in [(1, "40 days"), (2, "1 day")] {
        sqlx::query(&format!(
            "INSERT INTO events (id, space_id, session_id, seq, kind, payload, idempotency_key, occurred_at, received_at)
             VALUES ($1, $2, $3, $4, 'tool_use', $5, $6, now(), now() - interval '{age}')"
        ))
        .bind(new_id())
        .bind(s)
        .bind(session)
        .bind(seq)
        .bind(serde_json::json!({"text": big}))
        .bind(format!("k{seq}"))
        .execute(&e.pool)
        .await
        .unwrap();
    }
    for (key, age, status) in
        [("old-done", "100 days", "done"), ("old-pending", "100 days", "pending"), ("new-done", "1 day", "done")]
    {
        sqlx::query(&format!(
            "INSERT INTO work_packets (id, space_id, kind, prompt_ref, model_hint, inputs, output_schema, status, idempotency_key, created_at, done_at)
             VALUES ($1, $2, 'extract', 'p', 'sonnet', '{{}}', 'observer.v1', '{status}', $3, now() - interval '{age}',
                     CASE WHEN '{status}' = 'done' THEN now() - interval '{age}' END)"
        ))
        .bind(new_id())
        .bind(s)
        .bind(key)
        .execute(&e.pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO access_log (id, space_id, client_id, action, target_ids, created_at)
         VALUES ($1, $2, $3, 'get', '{}', now() - interval '200 days')",
    )
    .bind(new_id())
    .bind(s)
    .bind(e.who.client_id)
    .execute(&e.pool)
    .await
    .unwrap();

    for age in ["200 days", "1 day"] {
        sqlx::query(&format!(
            "INSERT INTO planner_runs (run_id, space_id, trigger, created_at)
             VALUES ($1, $2, 'nightly', now() - interval '{age}')"
        ))
        .bind(new_id())
        .bind(s)
        .execute(&e.pool)
        .await
        .unwrap();
    }

    let report = run_retention(&e.pool, &e.state.settings).await.unwrap();
    assert_eq!(report.events_trimmed, 1);
    assert_eq!(report.packets_deleted, 1);
    assert_eq!(report.logs_deleted, 2, "an access log row and a planner run");

    let (trimmed, kind, excerpt): (bool, String, String) =
        sqlx::query_as("SELECT payload_trimmed, payload->>'kind', payload->>'excerpt' FROM events WHERE seq = 1")
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert!(trimmed);
    assert_eq!(kind, "tool_use");
    assert_eq!(excerpt.chars().count(), 500);
    let untouched: bool =
        sqlx::query_scalar("SELECT NOT payload_trimmed FROM events WHERE seq = 2").fetch_one(&e.pool).await.unwrap();
    assert!(untouched);
    let left: Vec<String> =
        sqlx::query_scalar("SELECT idempotency_key FROM work_packets ORDER BY 1").fetch_all(&e.pool).await.unwrap();
    assert_eq!(left, ["new-done", "old-pending"]);

    // Idempotent.
    let again = run_retention(&e.pool, &e.state.settings).await.unwrap();
    assert_eq!((again.events_trimmed, again.packets_deleted, again.logs_deleted), (0, 0, 0));
}
