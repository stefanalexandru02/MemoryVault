//! Scoped `plan` jobs in the worker: run on their own lane's `job_pass`, restricted to their scope, and put
//! off while the scope was planned inside the minimum interval.

use chrono::{Duration, Utc};
use memvault_core::ids::new_id;
use memvault_core::jobs::enqueue_plan;
use memvault_core::schemas::common::{PlanTrigger, Scope, ScopeKind};
use memvault_worker::job_pass;
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{Env, env};

impl Env {
    /// A candidate fact old enough for the planner to activate on its own.
    async fn old_candidate(&self, scope_key: &str) -> Uuid {
        let (subject, fact) = (new_id(), new_id());
        sqlx::query("INSERT INTO entities (id, space_id, kind, canonical_name) VALUES ($1, $2, 'user', $3)")
            .bind(subject)
            .bind(self.who.space_id)
            .bind(format!("alex-{scope_key}"))
            .execute(&self.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text, confidence,
                                importance, status, first_seen_at, last_confirmed_at)
             VALUES ($1, $2, 'project', $3, $4, 'prefers.editor', '\"vim\"', 'vim', 0.7, 0.5, 'candidate',
                     now() - interval '2 days', now() - interval '2 days')",
        )
        .bind(fact)
        .bind(self.who.space_id)
        .bind(scope_key)
        .bind(subject)
        .execute(&self.pool)
        .await
        .unwrap();
        fact
    }

    async fn plan_later(&self, scope_key: &str, trigger: PlanTrigger) {
        let mut conn = self.pool.acquire().await.unwrap();
        let scope = Scope::new(ScopeKind::Project, scope_key);
        enqueue_plan(&mut conn, self.who.space_id, &scope, trigger).await.unwrap();
    }

    async fn status(&self, fact: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM facts WHERE id = $1").bind(fact).fetch_one(&self.pool).await.unwrap()
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_plan_lane_plans_the_scope_of_the_job(pool: PgPool) {
    let e = env(pool).await;
    let inside = e.old_candidate("acme").await;
    let outside = e.old_candidate("other").await;
    e.plan_later("acme", PlanTrigger::SessionEnd).await;

    assert_eq!(job_pass(&e.state, "w", &["plan"]).await.unwrap(), 1);
    assert_eq!(e.status(inside).await, "active");
    assert_eq!(e.status(outside).await, "candidate");
    assert_eq!(e.job_statuses().await, ["done"]);
    let runs: Vec<(String, String)> =
        sqlx::query_as("SELECT trigger, scope_key FROM planner_runs").fetch_all(&e.pool).await.unwrap();
    assert_eq!(runs, [("session_end".to_string(), "acme".to_string())]);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE type = 'plan' AND status = 'queued'")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_job_for_a_scope_planned_moments_ago_is_put_off(pool: PgPool) {
    let e = env(pool).await;
    let fact = e.old_candidate("acme").await;
    sqlx::query(
        "INSERT INTO scope_planning (space_id, scope_kind, scope_key, last_planned_at)
         VALUES ($1, 'project', 'acme', now() - interval '1 minute')",
    )
    .bind(e.who.space_id)
    .execute(&e.pool)
    .await
    .unwrap();
    e.plan_later("acme", PlanTrigger::Threshold).await;

    assert_eq!(job_pass(&e.state, "w", &["plan"]).await.unwrap(), 1);
    assert_eq!(e.status(fact).await, "candidate");
    let (status, attempts, run_after, locked): (String, i32, chrono::DateTime<Utc>, Option<String>) =
        sqlx::query_as("SELECT status, attempts, run_after, locked_by FROM jobs WHERE type = 'plan'")
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), attempts, locked), ("queued", 0, None));
    let wait = run_after - Utc::now();
    assert!(wait > Duration::minutes(13) && wait <= Duration::minutes(14), "{wait}");
    assert_eq!(job_pass(&e.state, "w", &["plan"]).await.unwrap(), 0, "not due yet");

    // a later trigger for the scope finds the waiting job and adds none
    e.plan_later("acme", PlanTrigger::SessionEnd).await;
    assert_eq!(e.job_statuses().await, ["queued"]);

    e.make_due().await;
    sqlx::query("UPDATE scope_planning SET last_planned_at = now() - interval '16 minutes'")
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(job_pass(&e.state, "w", &["plan"]).await.unwrap(), 1);
    assert_eq!(e.status(fact).await, "active");
    assert_eq!(e.job_statuses().await, ["done"]);
}
