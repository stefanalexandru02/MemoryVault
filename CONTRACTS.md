# MemoryVault build contracts (Rust)

The build contract is `docs/implementation-spec.md` (section numbers below refer to it); rationale is in
`docs/architecture.md`. This file records crate ownership, the cross-crate interfaces and the conventions every
crate codes against. Scope decided by the owner: **no agentic OS** (no Python client, no assistant observer mode);
the bash scripts of section 8 are the Rust binary `mem-hook`; the nightly runner is `mem-hook observe --all`.

## Running things

```bash
export PATH=/root/.cargo/bin:$PATH
cargo build --workspace
cargo test -p <your-crate>                       # needs the test Postgres at 127.0.0.1:55432 (postgres, trust)
cargo clippy -p <your-crate> --all-targets -- -D warnings
cargo fmt -p <your-crate>                         # rustfmt.toml: max_width 120

FAKE_TEI=1 DATABASE_URL=... DATABASE_SYSTEM_URL=... cargo run -p memvault -- serve --port 8000   # migrates first
cargo run -p memvault -- migrate                  # the same step by hand: DATABASE_SYSTEM_URL, else DATABASE_URL; or --dsn
```

`.cargo/config.toml` sets `DATABASE_URL=postgres://postgres@127.0.0.1:55432/postgres` for cargo; `#[sqlx::test]`
creates a fresh database per test from it. Agents share one checkout and one `target/`: "Blocking waiting for file
lock" is normal, wait; never kill cargo or delete `target/`. Build and test only your own crate.

## Workspace and ownership

| Crate (dir) | Package / lib | Owner | Owns |
|---|---|---|---|
| `crates/core` | `memvault-core` / `memvault_core` | foundation | config, db + RLS tx, ids, auth, schemas, TEI + FakeTei, prompts, appliers, invalidation, errors, AppState, test-support |
| `crates/server` | `memvault` (bin) / `memvault_server` | foundation | CLI, router assembly, `/health`, `/metrics`, `/mcp` mount, migrate |
| `crates/ingest` | `memvault-ingest` | ingest | sessions, events, windows, work packets (lease/result/fail/heartbeat), prompts route, extract/summary appliers |
| `crates/recall` | `memvault-recall` | recall | candidates, fusion, rerank, budgets, rendering, context cache + listener, recall/context/search/timeline/get/explain/history/by-file/feedback routes, `me/settings`, `stats/recall` |
| `crates/memories` | `memvault-memories` | memories | memories (remember/forget/feedback) + work-state routes, MCP server |
| `crates/consolidate` | `memvault-consolidate` | consolidation | planner, consolidation appliers, revert, admin routes and admin CLI |
| `crates/worker` | `memvault-worker` | worker | embedding parts and jobs, worker loop, sweeps, retention |
| `crates/evals` | `memvault-evals` | evals | record, redact, replay, compare |
| `crates/mem-hook` | `mem-hook` (bin) | plugin | hook subcommands + `observe`; **no dependency on core** (ureq + serde_json, no async runtime) |

Dependency graph: every crate depends on core; memories -> recall; worker -> ingest, recall. Do not add an edge
that creates a cycle (ingest and recall must not depend on worker or memories).

Foundation-owned shared files: root `Cargo.toml`, `Cargo.lock` (cargo updates it; fine), `.cargo/config.toml`,
`rustfmt.toml`, `.gitignore`, `crates/core/**`, `crates/server/**`, `migrations/0001-0099`, `schemas/*.json`,
`CONTRACTS.md`. Need a change there (a workspace dependency, a schema field, a setting, a column on a foundation
table, a core helper)? Put it in your result's `requests` with the exact change; do not edit.

Each module crate owns its `Cargo.toml` (add dependencies as `x.workspace = true`; a dependency that is not in
`[workspace.dependencies]` is a request), `build.rs`, `src/**`, `tests/**`, and its migration range.
`[workspace.dependencies]` already has: axum 0.8, tokio, tokio-util, tower, tower-http, http, http-body-util,
sqlx 0.8 (postgres, runtime-tokio, tls-rustls, uuid, chrono, json, migrate, macros), pgvector (sqlx), serde,
serde_json, jsonschema, serde_yaml, minijinja 3, rmcp 3.7 (server, macros, transport-streamable-http-server),
reqwest 0.13, clap 4, tracing, tracing-subscriber, metrics, metrics-exporter-prometheus, uuid (v7), hmac, sha2,
rand 0.10, hex, chrono, thiserror, anyhow, async-trait, futures, ureq 3; test: axum-test, wiremock, tempfile,
serde_urlencoded.

## Migrations

`migrations/NNNN_name.sql`, applied by `sqlx::migrate` in version order, **one transaction per file**, tracked in
`_sqlx_migrations` with a checksum: never edit an applied file, add a new one. `memvault_core::db::MIGRATOR`
embeds the directory at compile time. `serve` runs `db::migrate` on the system pool before it takes traffic (replicas
take turns on sqlx's advisory lock; migrations the binary does not know are ignored, so a rollback still starts);
`worker` and `planner` call `db::wait_for_schema`. `memvault migrate` is the same step by hand.

| Range | Owner |
|---|---|
| 0001-0099 | foundation (0099 = grants) |
| 0100-0199 | ingest (0100 exists: `session_windows`, lease bookkeeping) |
| 0200-0299 | recall (0200-0203 exist: context cache columns, invalidations, injection modes) |
| 0300-0399 | memories / mcp |
| 0400-0499 | consolidation (0400 exists: decay and revert bookkeeping) |
| 0500-0599 | worker |
| 0600-0699 | evals |
| 0900-0999 | hardening (runs last; narrows privileges granted by 0099) |

Rules for module migrations:
- New table with `space_id`: `SELECT memsvc_apply_space_rls('your_table');` (standard sel/ins/upd/del policies for
  `memsvc_app`, RLS enabled and forced); `memsvc_apply_space_rls('t', 'readable')` for log-style tables a reader
  must append to. The core RLS test fails for any table with a `space_id` column lacking forced RLS and a policy.
- Grants are automatic: `0099_grants.sql` sets `ALTER DEFAULT PRIVILEGES` so every table and sequence created by
  later migrations gets SELECT/INSERT/UPDATE/DELETE (sequences USAGE/SELECT/UPDATE) for `memsvc_app` and
  `memsvc_system`. No GRANT statements needed.
- Columns on another crate's or a foundation table: `ALTER TABLE ... ADD COLUMN IF NOT EXISTS ...` in your range.
  Generated columns need IMMUTABLE functions: use `memsvc_join(text[], sep)` instead of `array_to_string`.
- BM25 (`USING bm25`) only inside `DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_extension WHERE extname='pg_search') ...`.
  pg_search is not installed on the test cluster.
- Roles are cluster-wide and created by 0002 only when missing.

## Schema (migrations 0001-0011, 0099)

Full DDL of spec 4.1-4.7 plus:
- `clients.current_project_key`, `clients.current_project_at` (8.4 MCP project inference), `api_tokens.kind`.
- `events.payload_trimmed bool` (4.8).
- `search_tsv` generated tsvector on `observations` (spec) **and** on `summaries`, `facts`, `procedures` (keyword
  fallback, 11.2), each with a GIN index.
- `procedure_hints(id, space_id, scope_kind, scope_key, task_class, note, observation_id, session_id, packet_id,
  consumed_at, created_at)` - 10.4 step 4.
- `context_cache(cache_key PK, user_id, space_id, space_ids uuid[], scopes text[] ('kind:key'), device_id, source,
  body_md, etag, token_estimate, rendered_at, mode, hit_count)` - 11.6. RLS: a user sees only own rows.
- `user_settings(user_id PK, injection_mode 'full'|'lean', updated_at)` - the user's defaults. RLS: own row only
  (no `space_id`, so `memsvc_apply_space_rls` is not used).
- `rate_limits(subject, window_start, count)` - 5.1. Old windows are pruned by the worker's retention job.
- SQL functions: `app_user_id()`, `app_client_id()`, `readable_spaces()`, `writable_spaces()`,
  `memsvc_join(text[], text)`, `memsvc_apply_space_rls(tbl, write_check)`,
  `memsvc_invalidate_context(space uuid, kind text, key text) -> int` (SECURITY DEFINER; kind NULL = whole space).
- RLS (forced) on: sessions, events, observations, summaries, entities, facts, procedures, work_state, profiles,
  embeddings, work_packets, memory_audit, procedure_hints (writes need owner/writer); retrieval_log, access_log
  (writes need any membership); context_cache (own rows); user_settings (own row); memberships (own rows,
  read-only for the app role).
  users, spaces, clients, api_tokens, jobs, rate_limits have no RLS.
- Roles `memsvc_app` (NOINHERIT LOGIN) and `memsvc_system` (NOINHERIT LOGIN BYPASSRLS).
- `emb_hnsw` HNSW index (pgvector 0.6 is fine; `hnsw.iterative_scan` only on >= 0.8). BM25 indexes `obs_bm25`,
  `sum_bm25`, `fact_bm25`, `proc_bm25` only when pg_search exists (0011).

## Module migrations (0100-0699)

- **0100 ingest**: `session_windows(session_id, window_no, prompt_no, tool_uses, last_event_at, closed_at,
  close_reason, prompt_summary_pending, ...)` (one open window per session), `sessions.session_summary_pending`,
  `work_packets.lease_seconds`, `.heartbeats`, `.applied` (first application's counts, 9.3 idempotency).
- **0200 recall**: `context_cache.client_id`, `.project_key` (what a re-render needs); index on queued
  `render_context` jobs by `payload->>'cache_key'`; `memsvc_recall_invalidate(space, kind, key) RETURNS TABLE
  (cache_key, user_id, client_id, space_id, project_key, device_id, source, rendered_at)` (SECURITY DEFINER,
  deletes the matching blocks and returns them; NULL kind = whole space, NULL space = every space).
- **0201 recall**: `context_invalidations(space_id, scope, at)` (pruned after 2 h), written by
  `memsvc_recall_invalidate`; `memsvc_context_stale(space_ids, scopes, rendered_at) -> bool` (SECURITY DEFINER,
  executable by `memsvc_app`): a cached block is served only when no matching invalidation is newer than its
  `rendered_at`, which closes the render/NOTIFY race.
- **0202 recall**: forced RLS on `context_invalidations` (it has `space_id`; the app role has no grants on it).
- **0203 recall**: `user_settings`; `retrieval_log.tokens`, `.hits`, `.injection` (`full`|`lean`, context calls only)
  and an index on `(client_id, created_at)`; `context_cache.mode`, `.hit_count`; `memsvc_recall_invalidate` is
  recreated returning `mode` as well (same grants). The cache key is `v2` and includes the mode.
- **0500 worker**: `jobs.priority` (claim order; live jobs 0, `reindex` backfill 10).
- **0900 hardening**: `memsvc_app` loses INSERT/UPDATE/DELETE on `users`, `spaces`, `api_tokens`, `clients`
  (except `api_tokens.last_used_at` and `clients.last_seen_at/current_project_key/current_project_at`) and DELETE on
  `jobs`; the SECURITY DEFINER functions are executable by `memsvc_app`/`memsvc_system` only, not PUBLIC.
  Admin writes go through the system role, so they are unaffected.
- **0300 memories**: `memory_feedback(id, space_id, user_id, client_id, target_table, target_id, verdict, note,
  created_at)`, append-only, unique on `(user_id, target_id, verdict)` (a repeat is a no-op), forced space RLS
  (writes need owner/writer); `facts.disputed_at` (the planner's queue of facts whose net negative feedback
  crossed the threshold).
- **0400 consolidation**: `observations.decayed_at`, `facts.decayed_at` (incremental decay),
  `memory_audit.seq bigserial` (reverse-replay order), `memory_audit.reverts_audit_id`,
  `procedure_hints.consumed_run_id`, index on `work_packets((inputs->>'run_id'))`.

## Conventions

- **Queries**: runtime `sqlx::query` / `query_as` / `query_scalar` with `.bind`, never the `query!` macros.
  Helpers take `&mut PgConnection`; pass `&mut *tx`. Commit explicitly (`tx.commit().await?`); a dropped
  transaction rolls back.
- **RLS**: request handlers run every memory query inside `memvault_core::user_tx(&state.db.app, &identity)`.
  Broker/worker/planner/appliers run inside `memvault_core::system_tx(&state.db.system)`. Space resolution and
  memberships reads must run on a `user_tx` connection for the same user (the app role sees only its own
  membership rows) or on `system_tx`.
- **Keyword leg**: check `memvault_core::db::caps(&pool).await?.pg_search`; without it use
  `search_tsv @@ websearch_to_tsquery('simple', $q)` with `ts_rank_cd`. Vector leg: call
  `db::tune_vector_search(&mut tx, &caps, 80)` first (sets `hnsw.iterative_scan` only when supported).
- **Ids**: `memvault_core::ids::new_id()` (UUIDv7); `short_id(&id)` = last 6 hex chars for citation tags;
  `id_time(&id)`.
- **Time**: `timestamptz` / `chrono::DateTime<Utc>` everywhere.
- **token_estimate** = `ceil(chars / 4)`: `memvault_core::estimate_tokens(&str) -> i32` (replaces tiktoken).
- **Scopes**: `schemas::common::Scope { kind: ScopeKind, key }`; global key `""`; text form `kind:key`
  (`Scope::as_text`, `str::parse::<Scope>()`, `parse_scopes_param("global:,project:x")`). `ScopeKind` binds as the
  Postgres enum `scope_kind` and `Vec<ScopeKind>` as `scope_kind[]`.
- **String enums** (`EventKind`, `PacketKind`, `PacketStatus`, `OutputSchemaName`, `MemoryTable`,
  `EmbeddingOwnerTable`, `SummaryGrain`, `FactStatus`, `MembershipRole`, `SpaceKind`, `ClientKind`,
  `ObservationType`, `Concept`, `Sensitivity`, `ScopeHint`): serde in spec spelling, `as_str()`, `FromStr`,
  `Display`, `ALL`, and they bind to / decode from `text` columns (also `Vec<_>` as `text[]`).
- **Errors**: return `memvault_core::ApiError` (`ApiError::new(status, code, msg)`, `.retryable()`,
  `.with_extra(k, v)`, `.with_header(name, v)`; helpers `invalid` (422 `invalid_request`), `bad_request`,
  `unauthorized`, `forbidden(code, msg)`, `not_found`, `conflict`, `internal`, `not_implemented`). Rendered as
  `{"error": {"code", "message", "retryable", ...extra}}`. `From<sqlx::Error>`: 42501 (RLS/privilege) -> 403,
  23505 -> 409, RowNotFound -> 404, pool timeout/IO -> 503 `db_unavailable`, else 500. `From<TeiError>` -> 503
  `tei_unavailable`. `From<anyhow::Error>` (downcasts an inner ApiError). `From<OutputError>` -> 422
  `schema_invalid` with `retry_with`.
- **Routers**: each crate exposes `pub fn router() -> axum::Router<AppState>` with paths **without** `/v1`
  (`/sessions`, `/work/{id}/result`); the server nests the merged routers under `/v1`
  (`memvault_core::http::api_router`). Use `memvault_core::http::{Json, Query, Path}` (rejections in the spec error
  shape) instead of the axum originals, and call the request type's `validate()` after extraction.
- **Handler pattern**:
  ```rust
  async fn h(State(state): State<AppState>, identity: Identity, Json(body): Json<SessionCreate>)
      -> Result<impl IntoResponse, ApiError> {
      require_scope(&state, &identity, auth::EVENTS_WRITE).await?;  // 403 insufficient_scope, then rate limit
      body.validate()?;
      let mut tx = user_tx(&state.db.app, &identity).await?;
      let space = resolve_write_space(&mut tx, &identity, body.space_id).await?;
      ...; tx.commit().await?; Ok((StatusCode::CREATED, Json(out)))
  }
  ```
  Call `require_scope` once per request (it counts against the rate limit); check extra scopes with
  `identity.ensure(auth::ADMIN)?`.
- **Audit actor**: `identity.actor()` = `client:<client_id>`; packets `packet:<id>`; planner `planner`.
- **Embed jobs**: `jobs(type='embed', payload={"owner_table": t, "owner_id": "<uuid>"})` via
  `memvault_core::jobs::enqueue_embed(&mut tx, table, id)` (re-exported as `memvault_worker::embed::enqueue_embed`;
  ingest must use the core path).
- **Job types** (`jobs`, worker lanes): `embed` `{owner_table, owner_id, model?}` (`model` set by reindex; a job
  for another model than `EMBED_MODEL` fails permanently); `render_context` = `memvault_recall::cache::
  RenderContextJob {cache_key, user_id, client_id, project_key, device_id, source, mode}` (`mode` defaults to `full`
  for jobs queued before it existed), queued by the API's cache
  listener with `run_after = now() + CONTEXT_RERENDER_DEBOUNCE_S`, coalesced per `cache_key`, run by
  `memvault_recall::cache::rerender(&state, &payload) -> Result<bool, ApiError>` (false: user or client gone; the
  job is done either way); `reindex`; `retention` (`idempotency_key = retention:<UTC date>`, queued by the sweep).
  Only the API listener deletes cache rows; the worker never does.
- **Invalidation**: `memvault_core::invalidate::notify_invalidate(&mut tx, space_id, scope_kind, scope_key)` inside
  every write transaction that changes what context/recall shows; delivered on commit on channel
  `memory_invalidate` as `<space>:<kind>:<key>`; `parse_payload(s) -> Option<(Uuid, ScopeKind, String)>`.
- **Packet idempotency keys** (9.1): `extract:<session>:<window>`, `summarize_prompt:<session>:<prompt_no>`,
  `summarize_session:<session>`.
- **Metrics**: `metrics::counter!/histogram!/gauge!` with the `memsvc_` prefix; the recorder is
  `memvault_core::metrics::prometheus_handle()`. Core records `memsvc_tei_latency_seconds{service}` and
  `memsvc_tei_errors_total{service}`; do not redefine them.
- **Logging**: `tracing`; never log tokens; payloads only at DEBUG.

## Settings (`memvault_core::Settings::from_env()`, env vars without prefix)

`DATABASE_URL` (app role), `DATABASE_SYSTEM_URL` (system role; unset = the app URL, and `system_tx` errors unless
that login is superuser or BYPASSRLS), `DB_POOL_MAX` (10), `TEI_EMBED_URL`, `TEI_RERANK_URL`, `EMBED_MODEL`
(`bge-m3`, the value stored in `embeddings.model`), `EMBED_DIMS` (1024), `RERANK_MODEL`, `TEI_EMBED_TIMEOUT_S`
(2.0), `TEI_RERANK_TIMEOUT_S` (0.25), `FAKE_TEI` (bool), `PROMPTS_DIR` (`prompts`), `SCHEMAS_DIR` (`schemas`),
`TOKEN_PEPPER`, `RATE_LIMIT_EVENTS_PER_MIN` (600), `RATE_LIMIT_DEFAULT_PER_MIN` (120), `WINDOW_TOOL_USES` (8),
`WINDOW_IDLE_SECONDS` (90), `LEASE_SECONDS_DEFAULT` (600), `PREFERRED_RUNNER_SECONDS` (600),
`WORKER_SWEEP_INTERVAL_S` (30), `EMBED_BATCH_SIZE` (64), `EMBED_CONCURRENCY` (2), `EMBED_MAX_PART_TOKENS` (512),
`CONTEXT_RERENDER_DEBOUNCE_S` (2), `RETENTION_EVENTS_PAYLOAD_DAYS` (30), `RETENTION_PACKETS_DAYS` (90),
`RETENTION_LOGS_DAYS` (180), `HOST`, `PORT`, `LOG_LEVEL` (EnvFilter directive), `LOG_FORMAT` (`json`|`text`),
`TEI_EMBED_BATCH_TIMEOUT_S` (60, worker embed batches), `WORKER_METRICS_ADDR` (default `HOST:PORT`).
`serve` and `admin` refuse to start with an empty `TOKEN_PEPPER` (`Settings::require_token_pepper`).
Durations are `std::time::Duration` fields (`tei_rerank_timeout`, ...). Handlers read `state.settings`.

## Foundation interfaces (`memvault_core`)

### State and database
```rust
pub struct AppState { settings: Arc<Settings>, db: Db, tei: Arc<dyn Embedder>, prompts: Arc<PromptRegistry>,
                      schemas: Arc<OutputSchemas>, appliers: Arc<AppliersRegistry> }   // all fields pub, Clone
AppState::connect(settings, appliers).await -> anyhow::Result<AppState>; AppState::new(settings, db, appliers)
pub struct Db { app: PgPool, system: PgPool }  // Db::connect(&s), Db::connect_lazy(&s), Db::single(pool)
db::MIGRATOR: sqlx::migrate::Migrator
user_tx(&PgPool, &Identity).await -> Result<Transaction<'static, Postgres>, sqlx::Error>   // memsvc_app + app.user_id/app.client_id
system_tx(&PgPool).await -> Result<Transaction<'static, Postgres>, sqlx::Error>             // memsvc_system
db::caps(&PgPool).await -> Result<DbCaps, sqlx::Error>   // cached per pool: superuser, bypass_rls, current_user, pg_search, pgvector_version; .hnsw_iterative_scan()
db::tune_vector_search(&mut PgConnection, &DbCaps, ef_search: u32).await
```
The futures of `user_tx`, `system_tx` and `tune_vector_search` are `Send` with borrowed arguments (bound queries,
no `raw_sql(&format!(..))`), so handlers and spawned tasks await them directly; a core unit test pins this.

Pools per process (deploy): the API gets `DATABASE_URL` = `memsvc_app` **and** `DATABASE_SYSTEM_URL` =
`memsvc_system`, because it runs the work-packet broker (result application) and the `/v1/admin` routes in
`system_tx` after the route has authorised the caller (spec 4.7: broker = system role). Worker, planner and
migrate use `memsvc_system` for both. Every user-facing read and write runs in `user_tx` on the app pool.

### Auth (`memvault_core::auth`)
```rust
pub struct Identity { user_id, client_id, scopes: Vec<String>, default_space_id: Option<Uuid>, token_id,
                      token_kind: String, device_id: String, client_kind: String }
// axum extractor (FromRequestParts for any S with AppState: FromRef<S>): 401 on failure, cached in extensions
identity.has(scope) / identity.ensure(scope) -> Result<(), ApiError> / identity.actor()
require_scope(&AppState, &Identity, scope).await -> Result<(), ApiError>   // 403, then rate limit (events bucket for events:write)
authenticate_header(&AppState, Option<&str>).await -> Result<Identity, ApiError>  // for the MCP service
check_rate_limit(&PgPool, subject, limit).await -> Result<(), ApiError>  // 429 + Retry-After; fails open
resolve_read_spaces(&mut conn, &Identity, Option<&[Uuid]>) -> Result<Vec<Uuid>, ApiError>  // default space first, then shared; 403 space_forbidden
resolve_write_space(&mut conn, &Identity, Option<Uuid>) -> Result<Uuid, ApiError>         // 403 unless owner/writer
readable_space_ids(&mut conn, user_id) / writable_space_ids(&mut conn, user_id)
mint_token(&mut conn, user_id, client_id, TokenKind, pepper, MintOptions { scopes, expires_in, never_expires }) -> Result<MintedToken, ApiError>
verify_token(&mut conn, raw, pepper) -> Result<Identity, ApiError>; revoke_token(&mut conn, token_id) -> Result<bool, sqlx::Error>
auth::admin::{create_user(&mut conn, handle, display_name) -> CreatedUser { user_id, space_id },
              create_space(&mut conn, name, SpaceKind, owner_id), add_membership(&mut conn, space_id, user_id, MembershipRole),
              ensure_client(&mut conn, user_id, device_id, ClientKind, hostname, platform)}   // run on system_tx
```
Scope constants `EVENTS_WRITE`, `MEMORIES_READ`, `MEMORIES_WRITE`, `WORK_LEASE`, `ADMIN`. `TokenKind`
(`device` | `runner` | `admin`): `.scopes()`, `.ttl()` (365/90/30 days), `.client_kind()`. Token =
`mem_<8>_<32>` base62, stored as prefix + `HMAC-SHA256(TOKEN_PEPPER, secret)`. 401 codes: `unauthorized` (no
header), `invalid_token`, `token_revoked`, `token_expired`, `user_disabled`; 403 `insufficient_scope`,
`space_forbidden`; 429 `rate_limited`.

### Schemas (`memvault_core::schemas`)
`common` (enums above, `Scope`, `ErrorResponse`, `HealthResponse`, `estimate_tokens`, serde helpers `comma_list`
and `opt_from_str` for query strings), `sessions` (6.1), `events` (6.2, clamp constants), `recall` (6.3:
RecallRequest, RecallResponse, slot items with flattened `SlotItem`, ContextQuery, Feedback*), `memories`
(6.4/12.5: SearchQuery (comma-separated list params), IndexRow, SearchResponse, Timeline*, ByFileQuery, GetRequest,
GetResponse with `MemoryItem` tagged by `table`, ExplainResponse, HistoryResponse and HistoryEntry, `MemoryFeedbackRequest`/`MemoryFeedbackResponse`/`FeedbackCounts` and the
caution helpers `fact_caution`/`procedure_caution`, `RememberRequest` tagged by `kind`
(fact|observation|procedure), RememberResponse, ForgetQuery, ForgetResponse), `work_state` (6.5), `work` (6.6:
LeaseRequest, LeasedPacket with `prompt{system,user_template,user}`, ResultRequest, ResultResponse, Fail*,
HeartbeatResponse, `PacketRecord` = a `work_packets` row, `sqlx::FromRow`), `packets` (9.2 inputs: ExtractInputs,
SummarizePromptInputs, SummarizeSessionInputs), `outputs` (typed LLM outputs ObserverV1, SummaryV1,
ConsolidateFactsV1 (`FactDecision` tagged by `action`), ConsolidateSummaryV1, ConsolidateProceduresV1; `LlmOutput`),
`validate` (`OutputSchemas`), `admin` (6.7), `prompts` (PromptSpec, RenderedPrompt, PromptOut), `mcp` (tool
argument types, `MAX_RESULT_CHARS = 40000`). Request types with constraints have `validate() -> Result<(), ApiError>`.

### LLM output validation (spec 9.4)
`schemas/<name>.json` (JSON Schema 2020-12) for `observer.v1`, `summary.v1`, `consolidate.facts.v1`,
`consolidate.summary.v1`, `consolidate.procedures.v1`, loaded from `SCHEMAS_DIR` into `state.schemas`:
```rust
state.schemas.validate(packet.output_schema, &body.output) -> Result<LlmOutput, OutputError>
state.schemas.validate_value("observer.v1", &value) -> Result<serde_json::Value, OutputError>
schemas::validate::strip_code_fence(&str) -> &str
```
`output` may be the JSON object or the model's raw text (a JSON string): one surrounding code fence is stripped,
then parsed; it must be an object; schema errors (up to 10, with JSON pointers) form the `OutputError::Invalid`
text, which `ApiError::from` turns into 422 `schema_invalid` with `retry_with`. Unknown keys are allowed.

### Prompts (spec 10.1)
`state.prompts: PromptRegistry` loaded from `PROMPTS_DIR/*.yaml|yml` (missing dir = empty):
`get(name, version: Option<&str>) -> Result<&PromptSpec, PromptError>` (name may be `name@version`; `None` =
highest), `render(&spec, &inputs_json) -> Result<RenderedPrompt { system, user }, PromptError>`, `names()`,
`versions(name)`, `insert(spec)`. minijinja, strict undefined, no autoescape, trailing newline kept; `tojson`
emits plain JSON, `tojson(2)` indents. PromptSpec fields: `name` (`[a-z0-9_.-]+`), `version` (`v<n>`), `system`,
`user_template`, `output_schema`, `model_hint` (default `sonnet`), `description`.

### Appliers (spec 9.4)
```rust
#[async_trait] pub trait PacketApplier: Send + Sync {
    async fn apply(&self, conn: &mut PgConnection, packet: &PacketRecord, output: &LlmOutput) -> Result<AppliedCounts, ApiError>;
}
pub type AppliedCounts = BTreeMap<String, i64>;
AppliersRegistry::register(&mut self, PacketKind, impl PacketApplier + 'static); .get(kind); .kinds();
.apply(&mut conn, &packet, &output).await   // 500 when no applier is registered
```
The work-result route validates the output, then calls `state.appliers.apply` in the same `system_tx` that marks
the packet done. The server builds the registry with `memvault_ingest::register_appliers` and
`memvault_consolidate::register_appliers` (`memvault_server::all_appliers()`).

### TEI (spec 13)
```rust
#[async_trait] pub trait Embedder: Send + Sync {
    fn model(&self) -> &str; fn dims(&self) -> usize;
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, TeiError>;          // L2-normalised, input order
    async fn rerank(&self, query: &str, texts: &[String]) -> Result<Vec<f32>, TeiError>; // aligned with texts
    async fn health(&self) -> TeiHealth { embed, rerank };
}
TeiClient::new(embed_url, rerank_url, model, dims, embed_timeout, rerank_timeout) / ::from_settings(&s)
TeiError::{Timeout(String), Failed(String)}   // rerank timeout -> fall back to RRF, reranked=false
```
`state.tei` is `FakeTei` when `FAKE_TEI=1` (and always in tests).

### Server (`memvault_server`)
`build_router(state) -> Router`: `/v1` = merge of every crate's `router()`, `/mcp` =
`memvault_memories::mcp::service(state)` (nested service, 8 MB body limit), `/health` (DB `SELECT 1` with 2 s
timeout + TEI health: `ok` / `degraded` when TEI is down / 503 `down` when the DB is), `/metrics` (Prometheus),
JSON 404/405 fallbacks, `DefaultBodyLimit` 8 MB, request ids (`x-request-id`), tracing span, 60 s timeout.
`serve(state, TcpListener, shutdown) -> anyhow::Result<()>`: first `db::migrate`, then the router plus the context-cache listener
(`memvault_recall::cache::spawn_listener`) and recorder upkeep; `memvault serve` and the end-to-end test use it.
CLI `memvault`: `serve [--host] [--port]` (graceful shutdown on SIGINT/SIGTERM), `worker`
(`memvault_worker::run_worker_with(state, batch_embedder(&state), shutdown)`), `planner [--space-id]`,
`migrate [--dsn]`,
`admin <args...>` -> `memvault_consolidate::admin_cli::run` (including `admin doctor [--json]`, the checks of `GET /v1/admin/doctor`), `evals <args...>` -> `memvault_evals::cli::run`.

## Cross-crate signatures (fill in, keep the signatures)

| Signature | Crate / module | Spec |
|---|---|---|
| `pub fn router() -> Router<AppState>` | ingest, recall, memories, consolidate | 6 |
| `pub fn register_appliers(&mut AppliersRegistry)` | ingest, consolidate | 9.4 |
| `windows::close_idle_windows(pool: &PgPool, settings: &Settings) -> anyhow::Result<u64>` | ingest | 9.1 |
| `engine::recall(&AppState, &Identity, RecallRequest) -> Result<RecallResponse, ApiError>` | recall | 11 |
| `engine::search(&AppState, &Identity, SearchQuery) -> Result<SearchResponse, ApiError>` | recall | 6.4 |
| `engine::timeline(&AppState, &Identity, TimelineQuery) -> Result<TimelineResponse, ApiError>` | recall | 6.4 |
| `engine::get_memories(&AppState, &Identity, &[Uuid]) -> Result<GetResponse, ApiError>` | recall | 6.4 |
| `engine::explain(&AppState, &Identity, Uuid) -> Result<ExplainResponse, ApiError>` | recall | 12.5 |
| `engine::history(&AppState, &Identity, Uuid) -> Result<HistoryResponse, ApiError>` | recall | 6.4 |
| `context::render_context(&AppState, &Identity, project_key: Option<&str>, device_id: Option<&str>, source: &str) -> Result<String, ApiError>` | recall | 6.3, 11.5-11.7 |
| `cache::spawn_listener(AppState, CancellationToken) -> JoinHandle<()>` | recall | 11.6 |
| `cache::rerender(&AppState, &RenderContextJob) -> Result<bool, ApiError>` | recall | 11.6 |
| `mcp::service(AppState) -> axum::Router` (applies its own bearer auth; mount with `nest_service("/mcp", ..)`) | memories | 7 |
| `planner::run_planner(&AppState, Option<Uuid>) -> Result<ConsolidateResponse, ApiError>` | consolidate | 12.1 |
| `admin_cli::run(AppState, Vec<String>) -> anyhow::Result<()>` | consolidate | 5.2, 6.7 |
| `run_worker(AppState, CancellationToken) -> anyhow::Result<()>` (embeds via `state.tei`; tests) | worker | 14 |
| `run_worker_with(AppState, Arc<dyn Embedder>, CancellationToken)`, `batch_embedder(&AppState) -> Arc<dyn Embedder>` | worker | 13, 14 |
| `embed_pass(&AppState, worker: &str) -> anyhow::Result<usize>` (one batch of embed jobs) | worker | 13 |
| `embed::enqueue_embed(&mut PgConnection, owner_table: &str, owner_id: Uuid) -> Result<(), sqlx::Error>` (re-export of `memvault_core::jobs::enqueue_embed`, working) | worker | 13 |
| `cli::run(AppState, Vec<String>) -> anyhow::Result<()>` | evals | 16.2 |

Engine functions open their own `user_tx` (the MCP server calls them with the authenticated identity). The
admin/evals CLIs get an `AppState` whose pools connect lazily, with every applier registered.

## Tests

- Integration tests use `#[sqlx::test(migrations = "../../migrations")] async fn t(pool: PgPool)`: a fresh
  database with every migration. The pool is the superuser; `user_tx`/`system_tx` switch roles, so RLS and grants
  apply. Keep the crate's `build.rs` (`cargo:rerun-if-changed=../../migrations`) so new migrations re-embed.
- Prefer one integration-test binary per crate (`tests/<name>/main.rs` with `mod` files) to keep link time down.
- `memvault-core` feature `test-support` (`[dev-dependencies] memvault-core = { workspace = true, features =
  ["test-support"] }`), module `memvault_core::test_support`:

| Helper | What |
|---|---|
| `test_settings()` | FakeTei on, `TOKEN_PEPPER=test-pepper` (`TEST_PEPPER`), rate limits 1,000,000/min, repo `schemas/` and `prompts/` |
| `test_state(pool) -> AppState` | both roles on the test pool, `test_settings()`, FakeTei, no appliers |
| `test_state_with(pool, settings, appliers) -> (AppState, Arc<FakeTei>)` | custom settings/appliers, plus the fake's handle |
| `seed_identity(&pool, TokenKind) -> Seeded` | user + private default space (owner) + client + minted token; `Seeded { user_id, space_id, client_id, device_id, handle, token, token_id, identity }`, `.bearer()` |
| `seed_identity_with(&pool, kind, SeedOptions { handle, device_id, scopes })` | same with overrides |
| `seed_shared_space(&pool, name, owner, &[(user, MembershipRole)]) -> Uuid` | shared space with members |
| `app(state, router) -> Router` | your router nested under `/v1` with the server's fallbacks/body limit |
| `send(&app, Method, uri, Option<&token>, Option<json>) -> TestResponse` / `send_request(&app, Request)` | one in-process request; `TestResponse { status, headers, body }`, `.json()`, `.text()`, `.error_code()` |
| `repo_dir()` | repository root |

- **FakeTei** (`memvault_core::tei::FakeTei::new(model, dims)`, default `bge-m3`/1024): `embed` = hashed bag of
  words (sha256 of each lowercase `[alnum_]+` word picks a dimension and sign), L2-normalised, deterministic across
  runs, so texts sharing words have higher cosine; `rerank` = fraction of the query's distinct words present in
  the text (input order); `calls: Mutex<Vec<(&str, usize)>>` records `(method, n_texts)`; `set_down(true)` makes
  calls fail and `health` report down; `vector(text)` gives the embedding directly.
- TEI HTTP tests use `wiremock`.
- `crates/server/tests/e2e.rs` is the end-to-end test: `#[sqlx::test(migrations = false)]`, the `memvault` binary
  migrates and mints tokens, `serve` runs on a random port, and the real `mem-hook` binary (built with
  `cargo build -p mem-hook` from inside the test) plays a turn with a fake `claude` on PATH.

## Deviations from the spec (foundation)

- Migrations run by `sqlx::migrate` (`_sqlx_migrations`) instead of Alembic/the Python runner; the runner's grant
  post-step is `0099_grants.sql` with `ALTER DEFAULT PRIVILEGES`.
- `token_estimate = ceil(chars/4)` instead of tiktoken `cl100k_base` (11.5).
- Token hash is `HMAC-SHA256(TOKEN_PEPPER, secret)` (spec: sha256(secret)); token kind `agent` dropped (no agentic
  OS); `api_tokens.kind` column added.
- `users.default_space_id` has a deferrable FK to `spaces`; extra plain indexes on hot lookup paths.
- `search_tsv` added to summaries/facts/procedures for the tsvector fallback; generated columns use `memsvc_join`.
- RLS: `retrieval_log`/`access_log` writes need any membership; `context_cache` is per user; `memberships` has an
  own-rows SELECT policy for the app role.
- `PacketPrompt` carries `user` (rendered) besides `system`/`user_template` because the observer runner sends the
  rendered prompt (8.5).
- `observer.v1` observations accept an optional `scope_hint` (10.4 mentions it); `consolidate.procedures.v1`
  accepts `skip`/`skip_reason`; `consolidate.summary.v1` `period` is `{grain?, start, end}` (dates).
- Consolidation decision ids are strings as given in the packet inputs (not forced to UUID).
- Signatures vs. the task text: `close_idle_windows` also takes `&Settings` (idle threshold) and returns
  `anyhow::Result<u64>`; `render_context` returns `Result<String, ApiError>`; `enqueue_embed` lives in core
  (re-exported by the worker) because ingest cannot depend on the worker.
- Module routers declare paths without `/v1`; the server nests them.

## Deviations from the spec (modules)

Choices made while implementing, with the reason; the spec text stays as written.

- **Scope (owner decision)**: no Python client, no assistant observer mode, nothing whose only consumer is the
  agentic OS; the bash hooks of section 8 are the Rust binary `mem-hook` (`observe --all` is the nightly runner).
- **Summaries are deferred (6.1, 9.1)**: `summarize_prompt` and `summarize_session` are created once the packets
  they summarise have settled (done/failed/cancelled), because packet inputs are immutable and carry the window
  observations' titles; `windows::after_packet_settled` and the sweep trigger that. A Stop owes the former, `/end`
  the latter.
- **`attempts` counts at lease (6.6)**, not at `/fail` (see `queue.rs`): a runner that vanishes still uses an attempt.
- **mem-hook drops, not spools, 4xx rejections (8.3)** other than 401/403/404/408/425/429: retrying a request the
  server judged invalid can never succeed. An HTTP 500 is spooled but a replay skips it, counts the failure on the
  line (`replays`) and drops it after 5 (a poison request must not block the spool behind it).
- **`observe --final` leases up to 10 packets (8.5)**, not 3: the last run of a session also clears the windows of
  the session end; `--session` without `--final` keeps 3.
- **Same-value test of a fact candidate (10.4 step 3)** is jsonb equality or case-insensitive `value_text` equality;
  the "cosine > 0.95 after embedding" test is not done, because appliers do not get the embedder. Paraphrased
  confirmations land as conflict candidates and consolidation merges them.
- **Context cache hits write no `access_log` row (5.3, 4.6)**: a cache miss logs what the pack returned
  (`action = 'context'`), `POST /v1/recall` always logs (`'recall'`); the cached block carries no row ids to log
  on a hit. A hit does write a `retrieval_log` row (empty candidates and selection, the block's `hit_count`) so
  `GET /v1/stats/recall` counts every context call, and `tokens` there is the estimate of the text returned,
  live sections included.
- **Windows hook and static build (2, D5)**: mem-hook is `cfg(unix)`/`cfg(windows)` portable (it type-checks for
  `x86_64-pc-windows-gnu`; it has not been run on Windows). The Linux build is a static (`crt-static`) glibc
  binary via `install.sh`; a musl build needs `musl-tools` (see the plugin README).
- **Session counter after a pruned cache (8.3)**: mem-hook rebuilds `next_seq` from `POST /v1/sessions` (`next_seq`).
  If the cache is lost while the server is unreachable, a resumed session's offline events restart at the cached
  value and can collide with stored events until the server answers.
- **Search window**: `offset + limit` of `GET /v1/search` is capped at 1000.
- **`runner_kind` of `packets_leased_total`** is what the lease request reports (`runner_kind` field, `unknown`
  otherwise); the result `meta` is only known after the lease.
- **`evals replay --prompt`**: needs `--observer claude` (implied); only `extract` packets go to `claude -p`,
  summaries use the recorded or fake outputs.
- **Hard delete (5.3)** blanks `work_packets` whose `inputs`/`result`/`applied` contain the row's full id; content
  that was never tied to the id (a model's paraphrase in a result) cannot be found.
