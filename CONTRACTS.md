# MemoryVault build contracts

The build contract is `docs/implementation-spec.md` (section numbers below refer to it); rationale is in
`docs/architecture.md`. This file records the module ownership, the cross-module interfaces, and the
conventions every module codes against. Scope decided by the user: **no agentic OS** (no `clients/python`,
no assistant observer mode); the nightly runner is the plugin's `mem-observe --all` (8.6); push-mode
`/v1/recall` stays (it backs the SessionStart block and the MCP tools).

## Running things

```bash
cd server
uv sync                                   # venv in server/.venv
uv run pytest tests/<your_module> -q      # needs the test Postgres at 127.0.0.1:55432 (user postgres, trust)
uv run ruff check . && uv run ruff format --check .
uv run memsvc migrate                     # DATABASE_SYSTEM_URL, else DATABASE_URL; or --dsn
FAKE_TEI=1 DATABASE_URL=... uv run memsvc serve --port 8000
```

Tests: `server/tests/<module>/test_*.py`. No `__init__.py` is needed (pytest runs with
`--import-mode=importlib`, so equal test file names in different modules are fine; it also means test files
cannot `import conftest` - use fixtures). pytest-asyncio runs in auto mode with one session-wide event loop:
write plain `async def test_...`, no marks needed.

`tests/conftest.py` creates a fresh database `memsvc_test_<pid>` per pytest process, applies every migration in
`server/migrations`, and drops it at the end. Fixtures:

| Fixture | What |
|---|---|
| `settings` | `Settings` from the test env: `FAKE_TEI=1`, `TOKEN_PEPPER=test-pepper`, rate limits 1,000,000/min |
| `db` | `memsvc.db.Database` (session-scoped). Both pools connect as superuser `postgres`; `user_tx`/`system_tx` `SET LOCAL ROLE` to the real roles |
| `make_identity` | `await make_identity(kind="device"\|"runner"\|"admin", handle=None, device_id=None, scopes=None)` -> `Seeded(user_id, space_id, client_id, device_id, handle, token, token_id, identity, headers)`; creates user + private default space (owner) + client + minted token |
| `identity` | one `Seeded` device identity |
| `fake_tei` | a fresh `FakeTEI` (also installed as the global embedder by `app`) |
| `app` | `create_app(settings=settings, db=db, tei=fake_tei)` |
| `client` / `authed_client` | `httpx.AsyncClient` over ASGITransport, without / with `identity`'s bearer headers |
| `scratch_database` | `async with scratch_database("suffix") as dsn:` an extra empty database |

ASGITransport does not run lifespan events; `app.state.db`/`tei`/`settings` are set directly by `create_app`.

## Ownership

| Module | Owns |
|---|---|
| foundation | `server/pyproject.toml`, `memsvc/__init__.py`, `config.py`, `db.py`, `app.py`, `cli.py`, `ids.py`, `errors.py`, `migrate.py`, `invalidate.py`, `auth/**`, `schemas/**`, `embed/tei.py`, migrations `0001-0099`, `tests/conftest.py`, `tests/foundation/**`, `CONTRACTS.md`, `.gitignore` |
| ingest | `api/sessions.py`, `api/events.py`, `api/work.py`, `api/prompts.py`, `packets/**`, `prompts/registry.py` (+ `prompts/__init__.py`) |
| recall | `recall/**`, `api/recall.py` |
| memories | `api/memories.py`, `api/work_state.py`, `mcp/**` |
| consolidation | `consolidate/**`, `api/admin.py` |
| worker | `worker.py`, `embed/**` except `tei.py`, `metrics.py` (create it; must not redefine the two TEI metrics) |
| plugin | `clients/claude-code-plugin/**` |
| prompts-content | `server/prompts/*.yaml` |
| deploy | `deploy/**` |
| evals | `evals/**`, `memsvc/evals/**` |

Shared files belong to the foundation. Need a change there (a dependency, a schema field, a setting, a column
on a foundation table)? Put it in your result's `requests` with the exact change; do not edit.

### Migration number ranges

`server/migrations/NNNN_name.sql` (`name` = `[a-z0-9_]+`), applied in numeric order, one transaction per file,
tracked in `schema_migrations`. Additive only; never edit an applied file.

| Range | Module |
|---|---|
| 0001-0099 | foundation |
| 0100-0199 | ingest |
| 0200-0299 | recall |
| 0300-0399 | memories / mcp |
| 0400-0499 | consolidation |
| 0500-0599 | worker |
| 0600-0699 | evals |

Rules for module migrations:
- New table with `space_id`: call `SELECT memsvc_apply_space_rls('your_table');` (standard sel/ins/upd/del
  policies for `memsvc_app`, RLS enabled and forced). Use `memsvc_apply_space_rls('t', 'readable')` for
  log-style tables a reader must append to. `tests/foundation/test_rls.py` fails for any table with a
  `space_id` column that lacks forced RLS and a `memsvc_app` policy.
- Grants are automatic: after applying files the runner grants SELECT/INSERT/UPDATE/DELETE on all tables
  and sequences in `public` to `memsvc_app` and `memsvc_system`.
- Columns on another module's or the foundation's table: `ALTER TABLE ... ADD COLUMN IF NOT EXISTS ...` in
  your range. Generated columns need IMMUTABLE functions: use `memsvc_join(text[], sep)` instead of
  `array_to_string`.
- BM25 (`USING bm25`) only inside `DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_extension WHERE extname='pg_search') ...`.
  pg_search is not installed on the test cluster.

## Schema (migrations 0001-0011)

Full DDL of spec 4.1-4.7 plus:
- `clients.current_project_key`, `clients.current_project_at` (8.4 MCP project inference), `api_tokens.kind`.
- `events.payload_trimmed bool` (4.8).
- `search_tsv` generated tsvector on `observations` (spec) **and** on `summaries`, `facts`, `procedures`
  (keyword fallback, 11.2), each with a GIN index.
- `procedure_hints(id, space_id, scope_kind, scope_key, task_class, note, observation_id, session_id,
  packet_id, consumed_at, created_at)` - 10.4 step 4.
- `context_cache(cache_key PK, user_id, space_id, space_ids uuid[], scopes text[] ('kind:key'), device_id,
  source, body_md, etag, token_estimate, rendered_at)` - 11.6. RLS: a user sees only own rows.
- `rate_limits(subject, window_start, count)` - 5.1. Old windows are pruned by the worker's retention job.
- SQL functions: `app_user_id()`, `app_client_id()`, `readable_spaces()`, `writable_spaces()`,
  `memsvc_join(text[], text)`, `memsvc_apply_space_rls(tbl, write_check)`,
  `memsvc_invalidate_context(space uuid, kind text, key text) -> int` (SECURITY DEFINER; kind NULL = whole space).
- RLS (forced) on: sessions, events, observations, summaries, entities, facts, procedures, work_state,
  profiles, embeddings, work_packets, memory_audit, procedure_hints (writes need owner/writer);
  retrieval_log, access_log (writes need any membership); context_cache (own rows); memberships (own rows,
  read-only for the app role). users, spaces, clients, api_tokens, jobs, rate_limits have no RLS.
- Roles `memsvc_app` (NOINHERIT LOGIN) and `memsvc_system` (NOINHERIT LOGIN BYPASSRLS), created only if missing.
- `emb_hnsw` HNSW index (pgvector 0.6 is fine). BM25 indexes `obs_bm25`, `sum_bm25`, `fact_bm25`, `proc_bm25`
  only when pg_search exists (0011).

## Conventions

- **Ids**: UUIDv7 from `memsvc.ids.uuid7()`; `short_id(u)` = last 6 hex chars for citation tags.
- **Time**: `timestamptz` everywhere; Python `datetime` with tz.
- **token_estimate** = `ceil(chars / 4)` via `memsvc.schemas.common.estimate_tokens(text)` (replaces tiktoken).
- **Scopes**: `Scope(kind, key)`; global key is `""`; text form `kind:key` (query params, cache, NOTIFY).
  `parse_scopes_param("global:,project:x")`.
- **JSON/vector codecs**: `json`/`jsonb` params and results are Python objects (pass dicts, never
  `json.dumps` yourself; UUID/datetime/pydantic inside are serialised). `vector` is `list[float]`.
- **Errors**: raise `memsvc.errors.ApiError(status, code, message, retryable=False, extra={}, headers={})`;
  rendered as `{"error": {"code", "message", "retryable", **extra}}`. HTTPException/validation errors
  are converted to the same shape (validation: 422 `invalid_request` with `details`).
- **Audit actor**: `identity.actor` = `client:<client_id>`; packets `packet:<id>`; planner `planner`.
- **Routers**: declare full paths (`@router.post("/v1/sessions")`), no prefix.
- **Embed jobs**: `jobs(type='embed', payload={"owner_table": t, "owner_id": "<uuid>"})` via
  `memsvc.embed.jobs.enqueue_embed(conn, table, id)`.
- **Packet idempotency keys** (9.1): `extract:<session>:<window>`, `summarize_prompt:<session>:<prompt_no>`,
  `summarize_session:<session>`.

## Settings (`memsvc.config.get_settings()`, env vars without prefix)

`DATABASE_URL` (app role), `DATABASE_SYSTEM_URL` (system role; unset = `system_tx` raises), `DB_POOL_MIN/MAX`,
`MIGRATIONS_DIR`, `TEI_EMBED_URL`, `TEI_RERANK_URL`, `EMBED_MODEL` (`bge-m3`, the value stored in
`embeddings.model`), `EMBED_DIMS` (1024), `RERANK_MODEL`, `TEI_EMBED_TIMEOUT_S` (2.0), `TEI_RERANK_TIMEOUT_S`
(0.25), `FAKE_TEI` (bool), `PROMPTS_DIR` (`server/prompts`), `TOKEN_PEPPER`, `RATE_LIMIT_EVENTS_PER_MIN` (600),
`RATE_LIMIT_DEFAULT_PER_MIN` (120), `WINDOW_TOOL_USES` (8), `WINDOW_IDLE_SECONDS` (90), `LEASE_SECONDS_DEFAULT`
(600), `PREFERRED_RUNNER_SECONDS` (600), `WORKER_SWEEP_INTERVAL_S` (30), `EMBED_BATCH_SIZE` (64),
`EMBED_CONCURRENCY` (2), `EMBED_MAX_PART_TOKENS` (512), `CONTEXT_RERENDER_DEBOUNCE_S` (2),
`RETENTION_EVENTS_PAYLOAD_DAYS` (30), `RETENTION_PACKETS_DAYS` (90), `RETENTION_LOGS_DAYS` (180), `HOST`, `PORT`,
`LOG_LEVEL`. Read settings in request handlers from `request.app.state.settings`.

## Foundation interfaces

### Database - `memsvc.db`

```python
class Database: app: asyncpg.Pool; system: asyncpg.Pool | None; async def close()
async def connect(settings) -> Database
async with user_tx(db.app, user_id, client_id) as conn: ...   # memsvc_app, app.user_id/app.client_id set; RLS applies
async with system_tx(db.system) as conn: ...                   # memsvc_system, RLS bypass (worker, planner, broker)
caps(conn) -> DbCaps(superuser, current_user, pg_search: bool, pgvector_version: tuple | None, hnsw_iterative_scan)
async def tune_vector_search(conn, ef_search=80)               # SET LOCAL hnsw.ef_search (+ iterative_scan on >= 0.8)
```

In handlers: `db = request.app.state.db`. Space resolution and memberships reads must run on a `user_tx`
connection for the same user (the app role sees only its own membership rows) or on `system_tx`.

### Auth - `memsvc.auth`

```python
Identity(user_id, client_id, scopes: frozenset[str], default_space_id, token_id, token_kind, device_id, client_kind)
  .has(scope) / .actor
require_scope(*scopes) -> FastAPI dependency returning Identity   # 401/403, then rate limit (events bucket if events:write)
get_identity(request) -> Identity                                 # auth only, cached on request.state.identity
authenticate_header(db, settings, authorization_header) -> Identity   # for the MCP app
check_rate_limit(pool, subject, limit) -> None                    # 429 + Retry-After; fails open
resolve_read_spaces(conn, identity, requested=None) -> list[UUID] # default space first, then shared spaces; 403 on foreign ids
resolve_write_space(conn, identity, requested=None) -> UUID       # 403 unless owner/writer
readable_space_ids(conn, user_id) / writable_space_ids(conn, user_id)
mint_token(conn, *, user_id, client_id, kind, pepper, scopes=None, expires_in=None, never_expires=False) -> MintedToken
verify_token(conn, raw, pepper) -> Identity ; revoke_token(conn, token_id) -> bool
memsvc.auth.admin: create_user(conn, *, handle, display_name=None) -> CreatedUser(user_id, space_id)
                   create_space(conn, *, name, kind, owner_id) -> UUID ; add_membership(conn, *, space_id, user_id, role)
                   ensure_client(conn, *, user_id, device_id, kind="claude-code", hostname=None, platform=None) -> UUID
```

Scope constants: `EVENTS_WRITE`, `MEMORIES_READ`, `MEMORIES_WRITE`, `WORK_LEASE`, `ADMIN`. Token kinds
(`memsvc.auth.scopes.KIND_SCOPES`): device = events:write memories:read memories:write work:lease (365 d);
runner = work:lease memories:read (90 d); admin = admin + all (30 d). Token = `mem_<8>_<32>` base62,
stored as prefix + `HMAC-SHA256(TOKEN_PEPPER, secret)`. 401 codes: `unauthorized` (no header),
`invalid_token`, `token_revoked`, `token_expired`, `user_disabled`; 403 `insufficient_scope`,
`space_forbidden`; 429 `rate_limited`.

### Embedder - `memsvc.embed.tei`

```python
get_tei() -> Embedder        # process-wide; FakeTEI when FAKE_TEI=1. create_app() installs its tei via set_tei()
Embedder: model, dims, async embed(texts) -> list[list[float]], async rerank(query, texts) -> list[float] (input order),
          async health() -> {"embed": bool, "rerank": bool}, async aclose()
TEIClient(embed_url, rerank_url, *, model, dims, embed_timeout_s, rerank_timeout_s)
TEIError, TEITimeout(TEIError)   # rerank timeout -> fall back to RRF, reranked=false
FakeTEI(model, dims)             # hashed bag-of-words unit vectors (shared words => higher cosine);
                                 # rerank = fraction of query tokens in the text; .calls records invocations
```

Metrics `memsvc_tei_latency_seconds{service}` and `memsvc_tei_errors_total{service}` live in `tei.py`.

### Invalidation - `memsvc.invalidate`

`await notify_invalidate(conn, space_id, scope_kind, scope_key)` inside every write transaction that changes
what context/recall shows; delivered on commit on channel `memory_invalidate` as `<space>:<kind>:<key>`.
`parse_payload(s) -> (UUID, kind, key)`.

### Schemas - `memsvc.schemas`

`common` (literals: ScopeKind, EventKind, PacketKind, OutputSchemaName, ObservationType (code mode's nine),
Concept...; `Scope`, `ErrorResponse`, `HealthResponse`, `estimate_tokens`), `sessions` (6.1), `events` (6.2,
clamp constants), `recall` (6.3: RecallRequest, RecallResponse, slot items, ContextQuery, Feedback*),
`memories` (6.4/12.5: SearchQuery, IndexRow, SearchResponse, TimelineQuery/Response, ByFileQuery, GetRequest,
GetResponse with `MemoryItem` = Observation/Summary/Fact/ProcedureFull discriminated on `table`,
ExplainResponse, RememberRequest = RememberFact/Observation/Procedure discriminated on `kind`,
RememberResponse, ForgetResponse), `work_state` (6.5), `work` (6.6: LeaseRequest, LeasedPacket with
`prompt{system,user_template,user}`, ResultRequest, ResultResponse, FailRequest/Response, HeartbeatResponse,
PacketRecord = a work_packets row), `packets` (9.2 inputs: ExtractInputs, SummarizePromptInputs,
SummarizeSessionInputs), `outputs` (observer.v1, summary.v1, consolidate.facts.v1, consolidate.summary.v1,
consolidate.procedures.v1; `OUTPUT_SCHEMAS`, `validate_output(name, dict|str)` with fence stripping,
raising `OutputInvalid` whose text is the `retry_with`), `admin` (6.7), `prompts` (PromptSpec,
RenderedPrompt, PromptOut), `mcp` (tool argument models, `MAX_RESULT_CHARS = 40000`).

### App - `memsvc.app.create_app(settings=None, db=None, tei=None)`

Includes `router` from `memsvc.api.{sessions, events, work, prompts, recall, memories, work_state, admin}`;
routes `/mcp` (any method, path unchanged) to `memsvc.mcp.server.build_mcp_app(app.state)` and enters that
app's Starlette lifespan at startup; `/health` (DB `SELECT 1` + TEI health; `ok` / `degraded` when TEI is
down / 503 `down` when DB is down); `/metrics` (prometheus default registry). Startup hooks (skipped with a
warning while they raise NotImplementedError): `prompts.registry.load(settings.prompts_dir)` and
`await recall.cache.start_listener(db)` (task cancelled at shutdown).

CLI `memsvc`: `migrate [--dsn] [--dir]`, `serve [--host] [--port]`, `worker` (`worker.run_worker(settings)`),
`planner [--space-id]` (`consolidate.planner.run_planner(db, space_id)`), `admin ...` (argv after `admin`
to `consolidate.admin_cli.main(argv) -> int`), `evals ...` (`memsvc.evals.cli.main(argv) -> int`).

## Cross-module stubs (fill in, keep the signatures)

| Signature | File / owner | Spec |
|---|---|---|
| `async close_idle_windows(conn) -> int` | `packets/windows.py` / ingest | 9.1 |
| `register(kind, fn) -> fn`, `get(kind)`, `async apply(conn, packet: PacketRecord, output: BaseModel) -> dict[str, int]` (working) | `packets/appliers.py` / ingest | 9.4 |
| `async create_packet(conn, *, space_id, kind, prompt_ref, model_hint, output_schema, inputs, idempotency_key, session_id=None, preferred_client_id=None, available_after=None, max_attempts=3) -> UUID \| None` (working) | `packets/queue.py` / ingest | 9.1, 12.1 |
| `load(prompts_dir)`, `get(name, version=None) -> PromptSpec` (name may be `name@version`), `render(prompt, inputs) -> RenderedPrompt` | `prompts/registry.py` / ingest | 10.1 |
| `async recall(conn, identity, req: RecallRequest) -> RecallResponse` | `recall/engine.py` / recall | 11 |
| `async search(conn, identity, query: SearchQuery) -> SearchResponse` | `recall/engine.py` | 6.4 |
| `async timeline(conn, identity, query: TimelineQuery) -> TimelineResponse` | `recall/engine.py` | 6.4 |
| `async get_memories(conn, identity, ids: list[UUID]) -> GetResponse` | `recall/engine.py` | 6.4 |
| `async explain(conn, identity, memory_id: UUID) -> ExplainResponse` | `recall/engine.py` | 12.5 |
| `async by_file(conn, identity, query: ByFileQuery) -> SearchResponse` | `recall/engine.py` | 6.4 |
| `async render_context(conn, identity, project_key, device_id, source) -> str` | `recall/context.py` | 6.3, 11.5-11.7 |
| `async invalidate(conn, space_id, scope_kind, scope_key) -> int` (working), `async start_listener(db) -> Task \| None`, `async rerender(db, cache_keys) -> int` | `recall/cache.py` / recall | 11.6 |
| `parts_for(table, row) -> list[tuple[part, text]]` | `embed/parts.py` / worker | 13 |
| `async enqueue_embed(conn, table, id) -> None` (working) | `embed/jobs.py` / worker | 13 |
| `async run_worker(settings) -> None` | `worker.py` / worker | 14 |
| `async run_planner(db, space_id=None) -> ConsolidateResponse` | `consolidate/planner.py` / consolidation | 12.1 |
| `main(argv) -> int` | `consolidate/admin_cli.py` / consolidation | 5.2, 6.7 |
| `build_mcp_app(state) -> ASGIApp` | `mcp/server.py` / memories | 7 |
| `main(argv) -> int` | `memsvc/evals/cli.py` / evals | 16.2 |

"(working)" = a minimal real implementation is already in place so other modules are not blocked; the owner
may improve it.

## Deviations from the spec (foundation)

- Alembic replaced by `memsvc/migrate.py` (plain SQL files, `schema_migrations`, advisory lock).
- `token_estimate = ceil(chars/4)` instead of tiktoken `cl100k_base` (11.5).
- Token hash is `HMAC-SHA256(TOKEN_PEPPER, secret)` (the spec says sha256(secret); the pepper is the HMAC key).
- Token kind `agent` dropped (no agentic OS); kinds are device/runner/admin. `api_tokens.kind` column added.
- `users.default_space_id` has a deferrable FK to `spaces`; extra plain indexes on hot lookup paths.
- `search_tsv` added to summaries/facts/procedures for the tsvector fallback; generated columns use
  `memsvc_join` because `array_to_string` is not IMMUTABLE.
- RLS: `retrieval_log`/`access_log` writes need any membership (readers must log their reads);
  `context_cache` is per user; `memberships` gets an own-rows SELECT policy for the app role.
- `build_mcp_app(state)` takes the FastAPI `app.state` (the task text said `build_mcp_app()`).
- `PacketPrompt` carries `user` (rendered) besides `system`/`user_template` because mem-observe uses `.prompt.user` (8.5).
- `observer.v1` observations accept an optional `scope_hint` (10.4 mentions it); `consolidate.procedures.v1`
  accepts `skip`/`skip_reason`; `consolidate.summary.v1` `period` is `{grain?, start, end}`.
- Consolidation decision ids are strings as given in the packet inputs (not forced to UUID).
