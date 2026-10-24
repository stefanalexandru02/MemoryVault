//! memvault-recall: candidates, fusion, rerank, budgets, rendering, context cache, recall routes
//! (spec 6.3, 6.4, 11). Skeleton: the owner fills it in, keeping the public signatures.

use axum::Router;
use memvault_core::AppState;

/// Routes under `/v1` (paths without the prefix): recall, context, search, timeline, memories/get,
/// explain, observations/by-file, feedback.
pub fn router() -> Router<AppState> {
    Router::new()
}

pub mod engine {
    //! Entry points shared with memvault-memories (MCP tools). Each opens its own `user_tx` for
    //! the caller, so RLS bounds what is visible, and resolves space ids with
    //! `memvault_core::auth::resolve_read_spaces` (403 on a foreign space).

    use memvault_core::schemas::memories::{
        ExplainResponse, GetResponse, SearchQuery, SearchResponse, TimelineQuery, TimelineResponse,
    };
    use memvault_core::schemas::recall::{RecallRequest, RecallResponse};
    use memvault_core::{ApiError, AppState, Identity};
    use uuid::Uuid;

    /// Push-mode recall (spec 11).
    pub async fn recall(
        _state: &AppState,
        _identity: &Identity,
        _req: RecallRequest,
    ) -> Result<RecallResponse, ApiError> {
        todo!("memvault-recall: recall")
    }

    /// Index search (spec 6.4).
    pub async fn search(
        _state: &AppState,
        _identity: &Identity,
        _query: SearchQuery,
    ) -> Result<SearchResponse, ApiError> {
        todo!("memvault-recall: search")
    }

    /// Observations and summaries around an anchor, in time order (spec 6.4).
    pub async fn timeline(
        _state: &AppState,
        _identity: &Identity,
        _query: TimelineQuery,
    ) -> Result<TimelineResponse, ApiError> {
        todo!("memvault-recall: timeline")
    }

    /// Full bodies for up to 20 ids; writes `access_log` (spec 6.4).
    pub async fn get_memories(_state: &AppState, _identity: &Identity, _ids: &[Uuid]) -> Result<GetResponse, ApiError> {
        todo!("memvault-recall: get_memories")
    }

    /// Provenance chain (spec 12.5).
    pub async fn explain(_state: &AppState, _identity: &Identity, _id: Uuid) -> Result<ExplainResponse, ApiError> {
        todo!("memvault-recall: explain")
    }
}

pub mod context {
    use memvault_core::{ApiError, AppState, Identity};

    /// The SessionStart markdown block (spec 6.3 `GET /v1/context`, 11.5-11.7): profile,
    /// since-you-were-here (live), open work, procedures, recent observation index with legend.
    /// `source = "resume"` returns an empty string.
    pub async fn render_context(
        _state: &AppState,
        _identity: &Identity,
        _project_key: Option<&str>,
        _device_id: Option<&str>,
        _source: &str,
    ) -> Result<String, ApiError> {
        todo!("memvault-recall: render_context")
    }
}

pub mod cache {
    use memvault_core::AppState;
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    /// API-side `LISTEN memory_invalidate` loop deleting matching `context_cache` rows (spec
    /// 11.6). Started by `memvault serve`; stops when `shutdown` is cancelled. Stub: no-op task.
    pub fn spawn_listener(_state: AppState, shutdown: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move { shutdown.cancelled().await })
    }
}
