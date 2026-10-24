#![allow(dead_code)]
use std::sync::Arc;

use memvault_core::AppState;
use memvault_core::appliers::AppliersRegistry;
use memvault_core::auth::TokenKind;
use memvault_core::ids::new_id;
use memvault_core::tei::FakeTei;
use memvault_core::test_support::{Seeded, seed_identity, test_settings, test_state_with};
use sqlx::PgPool;
use uuid::Uuid;

pub struct Env {
    pub pool: PgPool,
    pub state: AppState,
    pub tei: Arc<FakeTei>,
    pub who: Seeded,
}

pub async fn env(pool: PgPool) -> Env {
    let who = seed_identity(&pool, TokenKind::Device).await;
    let (state, tei) = test_state_with(pool.clone(), test_settings(), AppliersRegistry::default());
    Env { pool, state, tei, who }
}

impl Env {
    pub async fn observation(&self, title: &str, narrative: Option<&str>, facts: &[&str]) -> Uuid {
        let id = new_id();
        let facts: Vec<String> = facts.iter().map(|f| f.to_string()).collect();
        sqlx::query(
            "INSERT INTO observations (id, space_id, scope_kind, scope_key, type, title, narrative, facts, extracted_by, content_hash)
             VALUES ($1, $2, 'project', 'proj-a', 'discovery', $3, $4, $5, '{}', $6)",
        )
        .bind(id)
        .bind(self.who.space_id)
        .bind(title)
        .bind(narrative)
        .bind(&facts)
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    pub async fn embed_job(&self, table: &str, id: Uuid) {
        let mut conn = self.pool.acquire().await.unwrap();
        memvault_worker::embed::enqueue_embed(&mut conn, table, id).await.unwrap();
    }

    pub async fn parts(&self, id: Uuid) -> Vec<String> {
        sqlx::query_scalar("SELECT part FROM embeddings WHERE owner_id = $1 ORDER BY part")
            .bind(id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }

    pub async fn job_statuses(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT status FROM jobs ORDER BY created_at, id").fetch_all(&self.pool).await.unwrap()
    }

    /// Make every queued job due now.
    pub async fn make_due(&self) {
        sqlx::query("UPDATE jobs SET run_after = now() - interval '1 second'").execute(&self.pool).await.unwrap();
    }
}
