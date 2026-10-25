//! The MCP server (spec 7): streamable HTTP, stateless, behind the same bearer auth as REST.
//!
//! [`service`] is mounted by the server at `/mcp`. A middleware authenticates the `Authorization`
//! header and hands the `Identity` to the tools through the request extensions. Tools return
//! markdown; failures come back as tool errors (`isError`) so the model can correct the call.
//! Project scope comes from the `X-Memory-Project` header when sent, else from
//! `clients.current_project_key`.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use memvault_core::auth::{MEMORIES_READ, MEMORIES_WRITE, authenticate_header};
use memvault_core::schemas::common::{Concept, MemoryTable, ObservationType, Scope, ScopeKind, Sensitivity};
use memvault_core::schemas::mcp::{
    MAX_RESULT_CHARS, McpScope, MemoryExplainArgs, MemoryFeedbackArgs, MemoryForgetArgs, MemoryGetArgs,
    MemoryHistoryArgs, MemoryRememberArgs, MemorySearchArgs, MemoryTimelineArgs, RememberKind, RememberScope,
    WorkStateWriteArgs,
};
use memvault_core::schemas::memories::{
    FactSubject, MemoryFeedbackRequest, RememberFact, RememberObservation, RememberProcedure, RememberRequest,
    SearchQuery, TimelineQuery,
};
use memvault_core::schemas::work_state::WorkStateWrite;
use memvault_core::{ApiError, AppState, Identity, require_scope};
use memvault_recall::{engine, render};
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData as McpError, Implementation,
    JsonObject, ListToolsResult, MetaObject, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::feedback::feedback;
use crate::memories::{current_project, forget, remember};
use crate::work_state;
use memvault_core::user_tx;

/// Header the plugin may set to name the project of the request.
const PROJECT_HEADER: &str = "x-memory-project";
/// Result caps in characters (4 characters per estimated token): search, timeline and explain ~2k
/// tokens, get ~6k.
const INDEX_CAP_CHARS: usize = 8_000;
const GET_CAP_CHARS: usize = 24_000;
const NEXT_STEP_HINT: &str = "\n\n_Next: `memory_timeline` for the neighbours of a row, `memory_get` for full bodies (ids above), `memory_history` for how a fact changed._";

/// The streamable-HTTP MCP service mounted by the server at `/mcp` (stateless: a new server value
/// per request, no session affinity; same bearer auth as REST via `authenticate_header`).
pub fn service(state: AppState) -> Router {
    let server_state = state.clone();
    // The bearer token guards the endpoint, so any Host header is accepted (tailnet hostnames).
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .disable_allowed_hosts();
    let mcp = StreamableHttpService::new(
        move || Ok(MemoryServer { state: server_state.clone() }),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new().fallback_service(mcp).layer(middleware::from_fn_with_state(state, authenticate))
}

/// 401 in the spec error shape unless the bearer token is valid; the identity rides in the request
/// extensions (rmcp copies them to the tool context).
async fn authenticate(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let header = req.headers().get(AUTHORIZATION).and_then(|v| v.to_str().ok());
    match authenticate_header(&state, header).await {
        Ok(identity) => {
            req.extensions_mut().insert(identity);
            next.run(req).await
        }
        Err(e) => e.into_response(),
    }
}

#[derive(Clone)]
struct MemoryServer {
    state: AppState,
}

/// Who is calling, and the project named in the request headers.
struct Caller {
    identity: Identity,
    project_header: Option<String>,
}

impl Caller {
    fn from_context(ctx: &RequestContext<RoleServer>) -> Result<Self, McpError> {
        let parts = ctx
            .extensions
            .get::<http::request::Parts>()
            .ok_or_else(|| McpError::internal_error("request context is missing", None))?;
        let identity = parts
            .extensions
            .get::<Identity>()
            .cloned()
            .ok_or_else(|| McpError::internal_error("request is not authenticated", None))?;
        let project_header = parts
            .headers
            .get(PROJECT_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from);
        Ok(Self { identity, project_header })
    }
}

type ToolResult = Result<String, ApiError>;

impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("memvault", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Shared memory across devices. Search first (memory_search), then memory_timeline around a hit, \
                 then memory_get for the full bodies you need.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let caller = Caller::from_context(&context)?;
        let args = Value::Object(request.arguments.unwrap_or_default());
        let outcome: ToolResult = async {
            match request.name.as_ref() {
                "memory_search" => self.search(&caller, parse(args)?).await,
                "memory_timeline" => self.timeline(&caller, parse(args)?).await,
                "memory_get" => self.get(&caller, parse(args)?).await,
                "memory_explain" => self.explain(&caller, parse(args)?).await,
                "memory_history" => self.history(&caller, parse(args)?).await,
                "memory_remember" => self.remember(&caller, parse(args)?).await,
                "memory_forget" => self.forget(&caller, parse(args)?).await,
                "memory_feedback" => self.feedback(&caller, parse(args)?).await,
                "work_state_write" => self.work_state_write(&caller, parse(args)?).await,
                other => Err(ApiError::invalid(format!("unknown tool {other}"))),
            }
        }
        .await;
        let result = match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("error ({}): {}", e.code, e.message))]),
        };
        Ok(result.into())
    }
}

/// Deserialize tool arguments; a bad shape is a tool error the model can read.
fn parse<T: DeserializeOwned>(args: Value) -> Result<T, ApiError> {
    serde_json::from_value(args).map_err(|e| ApiError::invalid(format!("invalid arguments: {e}")))
}

impl MemoryServer {
    /// The project of the request: header, else the client's current project.
    async fn project(&self, caller: &Caller) -> Result<Option<String>, ApiError> {
        if let Some(p) = &caller.project_header {
            return Ok(Some(p.clone()));
        }
        let mut tx = user_tx(&self.state.db.app, &caller.identity).await?;
        let key = current_project(&mut tx, &caller.identity).await?;
        tx.commit().await?;
        Ok(key)
    }

    async fn search(&self, caller: &Caller, args: MemorySearchArgs) -> ToolResult {
        require_scope(&self.state, &caller.identity, MEMORIES_READ).await?;
        args.validate()?;
        // The plugin sends the project header; with it, an unspecified scope means that project.
        let scope =
            args.scope.unwrap_or(if caller.project_header.is_some() { McpScope::Project } else { McpScope::All });
        let scopes = match scope {
            McpScope::All => None,
            McpScope::Global => Some(vec![Scope::global()]),
            // Without a known project there is nothing to narrow to.
            McpScope::Project => self.project(caller).await?.map(|key| vec![Scope::new(ScopeKind::Project, key)]),
        };
        let as_of = args.as_of.as_deref().map(parse_as_of).transpose()?;
        // The past is asked about facts and procedures as much as about what happened.
        let tables = as_of.map(|_| MemoryTable::ALL.to_vec());
        let query = SearchQuery {
            q: Some(args.query),
            scopes,
            types: args.types,
            tables,
            since: args.since.as_deref().map(parse_since).transpose()?,
            files: args.files,
            as_of,
            limit: args.limit,
            ..SearchQuery::default()
        };
        let rows = engine::search(&self.state, &caller.identity, query).await?.rows;
        let hint = if rows.is_empty() { "" } else { NEXT_STEP_HINT };
        Ok(cap(format!("{}{hint}", render::index_rows_markdown(&rows)), INDEX_CAP_CHARS))
    }

    async fn timeline(&self, caller: &Caller, args: MemoryTimelineArgs) -> ToolResult {
        require_scope(&self.state, &caller.identity, MEMORIES_READ).await?;
        args.validate()?;
        let anchor_id = self.resolve_one(caller, &args.anchor_id).await?;
        let query = TimelineQuery { anchor_id, before: args.before, after: args.after };
        let rows = engine::timeline(&self.state, &caller.identity, query).await?.rows;
        Ok(cap(render::index_rows_markdown(&rows), INDEX_CAP_CHARS))
    }

    async fn get(&self, caller: &Caller, args: MemoryGetArgs) -> ToolResult {
        require_scope(&self.state, &caller.identity, MEMORIES_READ).await?;
        args.validate()?;
        let resolved = engine::resolve_refs(&self.state, &caller.identity, &args.ids).await?;
        let ids: Vec<Uuid> = resolved.iter().filter_map(|(_, id)| *id).collect();
        let mut unknown: Vec<String> = resolved.iter().filter(|(_, id)| id.is_none()).map(|(r, _)| r.clone()).collect();
        if ids.is_empty() {
            return Err(ApiError::not_found(format!("no memory matches {}", unknown.join(", "))));
        }
        let found = engine::get_memories(&self.state, &caller.identity, &ids).await?;
        unknown.extend(found.missing.iter().map(Uuid::to_string));
        if found.items.is_empty() {
            return Err(ApiError::not_found(format!("no memory matches {}", unknown.join(", "))));
        }
        let mut bodies: Vec<String> = found.items.iter().map(render::memory_item_markdown).collect();
        truncate_fairly(&mut bodies, GET_CAP_CHARS);
        let mut out = bodies.join("\n\n");
        if !unknown.is_empty() {
            out.push_str(&format!("\n\n_Not found: {}_", unknown.join(", ")));
        }
        Ok(out)
    }

    async fn explain(&self, caller: &Caller, args: MemoryExplainArgs) -> ToolResult {
        require_scope(&self.state, &caller.identity, MEMORIES_READ).await?;
        let id = self.resolve_one(caller, &args.id).await?;
        let explained = engine::explain(&self.state, &caller.identity, id).await?;
        Ok(cap(render::explain_markdown(&explained), INDEX_CAP_CHARS))
    }

    async fn history(&self, caller: &Caller, args: MemoryHistoryArgs) -> ToolResult {
        require_scope(&self.state, &caller.identity, MEMORIES_READ).await?;
        let id = self.resolve_one(caller, &args.id).await?;
        let chain = engine::history(&self.state, &caller.identity, id).await?;
        Ok(cap(render::history_markdown(&chain), INDEX_CAP_CHARS))
    }

    async fn remember(&self, caller: &Caller, args: MemoryRememberArgs) -> ToolResult {
        require_scope(&self.state, &caller.identity, MEMORIES_WRITE).await?;
        args.validate()?;
        let project = self.project(caller).await?;
        let scope = match args.scope {
            Some(RememberScope::Global) => Scope::global(),
            Some(RememberScope::Project) => Scope::new(
                ScopeKind::Project,
                project.ok_or_else(|| ApiError::invalid("no current project is known; use scope \"global\""))?,
            ),
            None => project.map_or_else(Scope::global, |key| Scope::new(ScopeKind::Project, key)),
        };
        let content = args.content.trim().to_string();
        let first_line = content.lines().next().unwrap_or_default().trim().to_string();
        let request = match args.kind {
            RememberKind::Fact => {
                let predicate = args
                    .predicate
                    .ok_or_else(|| ApiError::invalid("a fact needs a predicate, e.g. \"prefers.editor\""))?;
                RememberRequest::Fact(RememberFact {
                    scope: Some(scope),
                    space_id: None,
                    sensitivity: Sensitivity::Normal,
                    subject: FactSubject { kind: "user".into(), name: args.subject.unwrap_or_else(|| "user".into()) },
                    predicate,
                    value: Value::Null,
                    value_text: content,
                    pinned: false,
                })
            }
            RememberKind::Note => RememberRequest::Observation(RememberObservation {
                scope: Some(scope),
                space_id: None,
                sensitivity: Sensitivity::Normal,
                obs_type: ObservationType::Discovery,
                title: ellipsize(&first_line, 120),
                subtitle: None,
                narrative: Some(content),
                facts: Vec::new(),
                concepts: Vec::<Concept>::new(),
                files: Vec::new(),
                importance: 0.7,
            }),
            RememberKind::Procedure => {
                let name = args.subject.unwrap_or_else(|| ellipsize(&first_line, 100));
                RememberRequest::Procedure(RememberProcedure {
                    scope: Some(scope),
                    space_id: None,
                    sensitivity: Sensitivity::Normal,
                    task_class: args.predicate.unwrap_or_else(|| "general".into()),
                    trigger_text: name.clone(),
                    name,
                    body_md: content,
                })
            }
        };
        let saved = remember(&self.state, &caller.identity, request).await?;
        Ok(format!("Remembered {} `{}`. It will show up as:\n{}", saved.table, saved.id, saved.rendered))
    }

    async fn forget(&self, caller: &Caller, args: MemoryForgetArgs) -> ToolResult {
        require_scope(&self.state, &caller.identity, MEMORIES_WRITE).await?;
        let id = self.resolve_one(caller, &args.id).await?;
        let done = forget(&self.state, &caller.identity, id, false, args.reason.as_deref()).await?;
        let action = serde_json::to_value(done.action).ok().and_then(|v| v.as_str().map(String::from));
        Ok(format!("Forgot {} `{}` ({}).", done.table, done.id, action.unwrap_or_default()))
    }

    async fn feedback(&self, caller: &Caller, args: MemoryFeedbackArgs) -> ToolResult {
        require_scope(&self.state, &caller.identity, MEMORIES_WRITE).await?;
        let id = self.resolve_one(caller, &args.id).await?;
        let request = MemoryFeedbackRequest { verdict: args.verdict, note: args.note };
        let done = feedback(&self.state, &caller.identity, id, request).await?;
        if !done.applied {
            return Ok(format!("You already gave {} on {} `{}`; nothing changed.", done.verdict, done.table, done.id));
        }
        let confidence = done.confidence.map(|c| format!(" Confidence is now {c:.2}.")).unwrap_or_default();
        let queued =
            if done.queued { " It is queued for the consolidation planner to retract or replace." } else { "" };
        Ok(format!("Recorded {} on {} `{}`.{confidence}{queued}", done.verdict, done.table, done.id))
    }

    async fn work_state_write(&self, caller: &Caller, args: WorkStateWriteArgs) -> ToolResult {
        require_scope(&self.state, &caller.identity, MEMORIES_WRITE).await?;
        args.validate()?;
        let scope = self.project(caller).await?.map(|key| Scope::new(ScopeKind::Project, key));
        let write = WorkStateWrite { scope, space_id: None, list_name: args.list.clone(), fields: args.fields };
        let lists = work_state::write(&self.state, &caller.identity, write).await?.lists;
        if lists.is_empty() {
            return Ok(format!("List `{}` is now empty.", args.list));
        }
        Ok(lists.iter().map(render::work_state_line).collect::<Vec<_>>().join("\n"))
    }

    /// A full id, `obs#a1b2c3` or the 6-hex short id as shown in the index, to one visible row id.
    async fn resolve_one(&self, caller: &Caller, reference: &str) -> Result<Uuid, ApiError> {
        let resolved = engine::resolve_refs(&self.state, &caller.identity, &[reference.to_string()]).await?;
        resolved
            .into_iter()
            .find_map(|(_, id)| id)
            .ok_or_else(|| ApiError::not_found(format!("no single memory matches {reference}")))
    }
}

// ---------- output helpers ----------

/// Cut `text` to `max_chars`, at a line end when possible, with a marker.
fn cap(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut cut: String = text.chars().take(max_chars).collect();
    if let Some(i) = cut.rfind('\n') {
        cut.truncate(i);
    }
    cut.push_str("\n\n_[truncated]_");
    cut
}

/// Shorten the longest bodies so the total fits `budget` chars; short bodies stay whole.
fn truncate_fairly(bodies: &mut [String], budget: usize) {
    let mut order: Vec<usize> = (0..bodies.len()).collect();
    order.sort_by_key(|&i| bodies[i].chars().count());
    let (mut remaining, mut left) = (budget, bodies.len());
    for i in order {
        let share = remaining / left;
        let len = bodies[i].chars().count();
        if len > share {
            let keep = share.saturating_sub(40);
            bodies[i] = format!(
                "{}\n…[truncated; fetch this id alone for the rest]",
                bodies[i].chars().take(keep).collect::<String>()
            );
        }
        remaining = remaining.saturating_sub(len.min(share));
        left -= 1;
    }
}

fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// RFC 3339, a date (`2026-10-01`) or a span back from now (`24h`, `7d`, `2w`).
fn parse_since(text: &str) -> Result<DateTime<Utc>, ApiError> {
    let text = text.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(text) {
        return Ok(t.with_timezone(&Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Ok(d.and_time(chrono::NaiveTime::MIN).and_utc());
    }
    if let Some((digits, unit)) = text.split_at_checked(text.len().saturating_sub(1))
        && let Ok(n) = digits.parse::<i64>()
        && n >= 0
    {
        let span = match unit {
            "h" => Duration::try_hours(n),
            "d" => Duration::try_days(n),
            "w" => Duration::try_weeks(n),
            _ => None,
        };
        if let Some(span) = span {
            return Ok(Utc::now() - span);
        }
    }
    Err(ApiError::invalid("since must be an RFC 3339 time, a date like 2026-10-01 or a span like 7d"))
}

/// RFC 3339 time for `as_of`.
fn parse_as_of(text: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(text.trim())
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| ApiError::invalid("as_of must be an RFC 3339 time like 2026-10-01T12:00:00Z"))
}

// ---------- tool definitions ----------

fn tools() -> Vec<Tool> {
    let read = ToolAnnotations::new().read_only(true);
    let write = ToolAnnotations::new().read_only(false).destructive(false);
    vec![
        tool(
            "memory_search",
            "Search shared memory for a compact index of matching observations and summaries (ids, dates, titles, token costs), or with types [\"document\"] the ingested repository docs (ADRs, READMEs, runbooks); use it first, then memory_timeline around a hit, then memory_get for full bodies (search → timeline → get).",
            json!({
                "query": {"type": "string", "description": "What to look for."},
                "types": {"type": "array", "items": {"type": "string"}, "description": "Observation types, e.g. decision, bugfix, or document for ingested docs."},
                "scope": {"type": "string", "enum": ["project", "global", "all"], "description": "Default: the current project when the client reports one, else all."},
                "since": {"type": "string", "description": "RFC 3339 time, a date like 2026-10-01, or a span like 7d."},
                "files": {"type": "array", "items": {"type": "string"}, "description": "Only observations touching these paths."},
                "as_of": {"type": "string", "description": "RFC 3339 time: search the memory as it stood then, facts and procedures included, with superseded ones back in force."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 30, "description": "Default 20."}
            }),
            &["query"],
            read.clone(),
        ),
        tool(
            "memory_timeline",
            "Show the index rows around an anchor id in time order, the second step of search → timeline → get.",
            json!({
                "anchor_id": {"type": "string", "description": "An id from memory_search."},
                "before": {"type": "integer", "minimum": 0, "maximum": 10, "description": "Default 5."},
                "after": {"type": "integer", "minimum": 0, "maximum": 10, "description": "Default 5."}
            }),
            &["anchor_id"],
            read.clone(),
        ),
        tool(
            "memory_get",
            "Fetch full memory bodies by id (at most 20), the last step of search → timeline → get; fetch only the ids you need.",
            json!({
                "ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 20, "description": "Ids from memory_search or memory_timeline."}
            }),
            &["ids"],
            read.clone(),
        ),
        tool(
            "memory_explain",
            "Show where a memory came from (session, events, extractor, audit history) for an id found by search → timeline → get.",
            json!({"id": {"type": "string", "description": "A memory id."}}),
            &["id"],
            read.clone(),
        ),
        tool(
            "memory_history",
            "Show how a fact or procedure changed over time: the chain of versions it replaced and was replaced by, with who changed it and why.",
            json!({"id": {"type": "string", "description": "A memory id from memory_search or memory_get."}}),
            &["id"],
            read,
        ),
        tool(
            "memory_remember",
            "Store a durable memory the user asked you to keep: a fact (needs a predicate like prefers.editor), a note, or a procedure.",
            json!({
                "kind": {"type": "string", "enum": ["fact", "note", "procedure"]},
                "content": {"type": "string", "description": "The text to remember (a fact's value is at most 300 characters)."},
                "scope": {"type": "string", "enum": ["project", "global"], "description": "Default: the current project, else global."},
                "subject": {"type": "string", "description": "Fact: who or what it is about (default user). Procedure: its name."},
                "predicate": {"type": "string", "description": "Fact: dotted lowercase key, e.g. prefers.editor. Procedure: its task class."}
            }),
            &["kind", "content"],
            write.clone(),
        ),
        tool(
            "memory_forget",
            "Soft-delete a memory by id (retract a fact, archive an observation, retire a procedure) after confirming it with search → timeline → get.",
            json!({
                "id": {"type": "string", "description": "The memory id."},
                "reason": {"type": "string", "description": "Why it is being forgotten."}
            }),
            &["id"],
            ToolAnnotations::new().read_only(false).destructive(true),
        ),
        tool(
            "memory_feedback",
            "Record that a recalled memory was helpful, wrong or outdated (once per verdict), which nudges a fact's confidence and, when negative feedback adds up, queues the fact to be retracted or replaced without deleting anything.",
            json!({
                "id": {"type": "string", "description": "The memory id, or the short id from the context block."},
                "verdict": {"type": "string", "enum": ["helpful", "wrong", "outdated"]},
                "note": {"type": "string", "description": "What was wrong or what is true now (at most 500 characters)."}
            }),
            &["id", "verdict"],
            write.clone(),
        ),
        tool(
            "work_state_write",
            "Update a named work list (such as todo) for the current project with key/value fields, where later writes win and a null value removes a key.",
            json!({
                "list": {"type": "string", "description": "List name, e.g. todo."},
                "fields": {"type": "object", "description": "Keys to set, e.g. {\"item-3\": \"done\"}; null removes a key."}
            }),
            &["list", "fields"],
            write,
        ),
    ]
}

fn tool(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
    hints: ToolAnnotations,
) -> Tool {
    let schema = json!({"type": "object", "properties": properties, "required": required});
    let Value::Object(schema) = schema else { unreachable!("json! built an object") };
    let mut meta = JsonObject::new();
    meta.insert("anthropic/maxResultSizeChars".into(), json!(MAX_RESULT_CHARS));
    Tool::new(name, description, Arc::new(schema)).with_meta(MetaObject(meta)).annotate(hints)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_cuts_at_a_line_end_and_marks_it() {
        assert_eq!(cap("short".into(), 100), "short");
        let text = "line one\nline two\nline three".to_string();
        assert_eq!(cap(text, 20), "line one\nline two\n\n_[truncated]_");
    }

    #[test]
    fn truncation_spares_short_bodies_and_fits_the_budget() {
        let mut bodies = vec!["a".repeat(100), "b".repeat(5_000), "c".repeat(5_000)];
        truncate_fairly(&mut bodies, 3_000);
        assert_eq!(bodies[0].len(), 100, "short bodies stay whole");
        assert!(bodies[1].contains("truncated") && bodies[2].contains("truncated"));
        assert!(bodies.iter().map(|b| b.chars().count()).sum::<usize>() <= 3_000 + 2 * 60);
        let mut fits = vec!["x".repeat(10), "y".repeat(10)];
        truncate_fairly(&mut fits, 1_000);
        assert!(fits.iter().all(|b| !b.contains("truncated")));
    }

    #[test]
    fn since_accepts_times_dates_and_spans() {
        assert_eq!(parse_since("2026-10-01T12:00:00Z").unwrap().to_rfc3339(), "2026-10-01T12:00:00+00:00");
        assert_eq!(parse_since("2026-10-01").unwrap().to_rfc3339(), "2026-10-01T00:00:00+00:00");
        let week_ago = Utc::now() - Duration::days(7);
        assert!((parse_since("7d").unwrap() - week_ago).num_seconds().abs() < 5);
        assert!((parse_since("2w").unwrap() - (Utc::now() - Duration::weeks(2))).num_seconds().abs() < 5);
        for bad in ["", "yesterday", "7x", "-3d", "d"] {
            assert!(parse_since(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn as_of_takes_rfc3339_only() {
        assert_eq!(parse_as_of("2026-10-01T12:00:00+02:00").unwrap().to_rfc3339(), "2026-10-01T10:00:00+00:00");
        for bad in ["", "2026-10-01", "7d", "last week"] {
            assert!(parse_as_of(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ellipsize_counts_characters() {
        assert_eq!(ellipsize("héllo", 10), "héllo");
        assert_eq!(ellipsize("héllo wörld", 6), "héllo…");
    }
}
