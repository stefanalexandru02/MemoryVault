//! Applying `summarize_prompt` / `summarize_session` results (spec 10.5): one `summaries` row of
//! grain `prompt` / `session`, embedded, with the context cache invalidated.

use async_trait::async_trait;
use memvault_core::ids::new_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::jobs::enqueue_embed;
use memvault_core::schemas::common::SummaryGrain;
use memvault_core::schemas::outputs::LlmOutput;
use memvault_core::schemas::work::PacketRecord;
use memvault_core::{ApiError, AppliedCounts, PacketApplier, estimate_tokens};
use serde_json::Value;
use sqlx::PgConnection;

use crate::extract::{extracted_by, packet_session, scope_for};

/// `summarize_prompt` (grain `prompt`) and `summarize_session` (grain `session`) packets.
#[derive(Debug, Clone, Copy)]
pub struct SummaryApplier {
    grain: SummaryGrain,
}

impl SummaryApplier {
    pub fn prompt() -> Self {
        Self { grain: SummaryGrain::Prompt }
    }

    pub fn session() -> Self {
        Self { grain: SummaryGrain::Session }
    }
}

#[async_trait]
impl PacketApplier for SummaryApplier {
    async fn apply(
        &self,
        conn: &mut PgConnection,
        packet: &PacketRecord,
        output: &LlmOutput,
    ) -> Result<AppliedCounts, ApiError> {
        let LlmOutput::Summary(out) = output else {
            return Err(ApiError::internal(format!("{} packet got a {} output", packet.kind, output.schema())));
        };
        let session = packet_session(conn, packet).await?;
        let scope = scope_for(None, &session);
        let prompt = self.grain == SummaryGrain::Prompt;
        let prompt_no = if prompt {
            packet.inputs.get("prompt_no").and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok())
        } else {
            None
        };
        let (period_start, period_end) =
            if prompt { (None, None) } else { (Some(session.started_at), session.ended_at) };
        let text: Vec<&str> = [
            out.request.as_deref(),
            out.investigated.as_deref(),
            out.learned.as_deref(),
            out.completed.as_deref(),
            Some(out.next_steps.as_str()),
            out.notes.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
        let id = new_id();
        sqlx::query(
            "INSERT INTO summaries (id, space_id, scope_kind, scope_key, grain, session_id, prompt_no, period_start,
                                    period_end, request, investigated, learned, completed, next_steps, notes,
                                    extracted_by, token_estimate)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
        )
        .bind(id)
        .bind(packet.space_id)
        .bind(scope.0)
        .bind(&scope.1)
        .bind(self.grain)
        .bind(session.id)
        .bind(prompt_no)
        .bind(period_start)
        .bind(period_end)
        .bind(&out.request)
        .bind(&out.investigated)
        .bind(&out.learned)
        .bind(&out.completed)
        .bind(&out.next_steps)
        .bind(&out.notes)
        .bind(extracted_by(packet))
        .bind(estimate_tokens(&text.join(" ")))
        .execute(&mut *conn)
        .await?;
        enqueue_embed(conn, "summaries", id).await?;
        notify_invalidate(conn, packet.space_id, scope.0, &scope.1).await?;
        Ok(AppliedCounts::from([("summaries".to_string(), 1)]))
    }
}
