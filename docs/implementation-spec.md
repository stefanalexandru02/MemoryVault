# Shared Memory Service — Implementation Specification

Status: draft for implementation. Companion to `memory-architecture.md`, which holds the rationale and the claude-mem comparison. This document is the build contract: schemas, protocols, exact flags, acceptance criteria.

---

## 0. Locked decisions

| # | Decision | Consequence for this spec |
|---|---|---|
| D1 | Postgres 17 + pgvector + ParadeDB `pg_search` is the only datastore. No Chroma, no SQLite, no Redis. | One schema, one backup, RLS covers vectors. Work queue is a Postgres table. |
| D2 | The cluster runs no model and stores no Claude credential. | All LLM calls happen on clients that already hold Claude: `claude -p` on Claude Code machines, the Agent SDK inside the agentic OS, a nightly runner for consolidation. The server builds, leases and validates work packets. |
| D3 | Clients are thin. | A Claude Code machine needs the plugin (hooks + one script), `curl`, `jq`, and the `claude` CLI it already has. The agentic OS needs the Python client. |
| D4 | Multi-user from the first migration. | `users`, `spaces`, `memberships`, `space_id` on every memory table, RLS everywhere. First deploy creates one user and one private space. |
| D5 | Language: Rust for the server and the Claude Code hook binary. | Decided by the owner after the Section 2 analysis, once the agentic OS was dropped from scope. Stack in Section 2. |
| D6 | Observer prompt ported from claude-mem `plugin/modes/code.json`; progressive-disclosure index format copied. | Observation quality is equal to claude-mem by construction; improvements come from context, retrieval and tiers. |
| D7 | Auth v1 is bearer tokens over a Tailscale-only ingress. OAuth is a later phase behind the same middleware. | Claude Code connects with `--header "Authorization: Bearer ${MEMORY_TOKEN}"`. |

---

## 1. Review of the architecture against these decisions

Checked end to end after the three revisions. Items that changed or needed tightening:

1. **Work queue split.** `jobs` (server-internal: embed, reindex, cache render) and `work_packets` (LLM work leased by clients) are now distinct tables. The earlier text used `jobs(extract)` for both.
2. **Recursion guard for the observer.** `claude -p` runs user and project hooks. `--bare` skips them but requires `ANTHROPIC_API_KEY`, so it cannot run on the subscription. The observer run therefore passes `--settings '{"disableAllHooks":true}'`, sets `MEMORY_OBSERVER=1`, and the hook script exits immediately when that variable is present. Both guards, always.
3. **Hook timeouts.** `SessionEnd` hooks share a 1.5 s budget by default. The `SessionEnd` hook only enqueues; it never waits on the network longer than 1 s.
4. **Token placement.** Claude Code stores MCP headers as plain JSON in `~/.claude.json`. Use `${MEMORY_TOKEN}` expansion so the file holds a reference, and keep the token in `~/.memory/config` (mode 0600) exported by the shell profile. Do not name the variable `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY`: Claude Code blanks those in MCP headers.
5. **MCP result caps.** Claude Code truncates MCP results above 25k tokens (default `MAX_MCP_OUTPUT_TOKENS`) and spills results over 50k characters to a file. Every tool result in this spec is budgeted under 8k tokens and `memory_get` refuses more than 20 ids per call.
6. **Keyword ranking.** `ts_rank_cd` is replaced by `pg_search` BM25 as the primary leg, with the generated tsvector kept as fallback when the extension is absent.
7. **Device on every row.** `sessions.client_id` already gives device provenance for everything derived from a session. Explicit writes (`memory_remember`) record the calling client on the row as `written_by_client_id`.

---

## 2. Language choice: Rust vs Python (vs Go)

The question is whether Rust makes the system more efficient. It does, in CPU and memory. It does not change the number that matters, and it costs the thing that is scarcest here: iteration speed on prompts, ranking and schemas during the first months.

### Where time goes in a recall (p50, warm cache, ~1M rows)

| Step | Python (FastAPI + asyncpg) | Rust (axum + sqlx) | Notes |
|---|---|---|---|
| Auth, parse, RLS `SET LOCAL` | 2–4 ms | 0.3–0.6 ms | Pydantic v2 validation is Rust-backed already. |
| Query embedding (TEI, CPU) | 15–25 ms | 15–25 ms | Network call, identical. |
| BM25 + vector legs (parallel, Postgres) | 10–40 ms | 10–40 ms | Postgres does the work, identical. |
| RRF, scoring, MMR over ≤ 60 candidates | 1–3 ms | < 0.2 ms | Trivial arithmetic. |
| Cross-encoder rerank, top 40 (TEI, CPU) | 100–150 ms | 100–150 ms | Network call, identical. |
| Render markdown, write `retrieval_log` | 2–5 ms | 0.5 ms | |
| **Total** | **~135–230 ms** | **~130–220 ms** | Rust saves roughly 5–8 ms, about 3%. |

### Where Rust wins clearly

| Property | Python | Rust |
|---|---|---|
| RSS per API replica | 150–250 MB | 20–40 MB |
| p99 under concurrent load | GC pauses and event-loop contention visible past ~200 req/s | Flat |
| Hook client startup | shell+curl ~10 ms, Python ~40–60 ms | static binary ~1–2 ms |
| Deployment unit | container with interpreter and deps | one static binary |

### Where Python wins clearly

- The agentic OS is Python. Packet schemas, observation schemas, the client, and the eval harness share one set of pydantic models. In Rust that is a second definition kept in sync by hand or by codegen.
- Prompt and ranking iteration is the dominant activity for the first three months. Python changes ship in minutes.
- The official `mcp` Python SDK has mature streamable-HTTP server support. The Rust `rmcp` SDK is official but younger.
- Postgres does the heavy lifting by design; the server is glue.

### Decision

**Rust for everything that runs**, decided by the owner. The agentic OS is out of scope, which removed the main argument for Python (shared models with a Python client). The efficiency gains that matter are the hook binary that runs on every tool call (about 1 ms startup, no `jq`, Windows support) and the small, flat-latency server footprint.

Stack:

| Concern | Crate | Notes |
|---|---|---|
| HTTP server | `axum` 0.8, `tokio`, `tower-http` | body limits, tracing, timeouts |
| Postgres | `sqlx` 0.8 (postgres, runtime-tokio, rustls, uuid, chrono, json, migrate) | runtime queries (`sqlx::query`/`query_as`), not compile-time macros, so builds need no database; `#[sqlx::test]` gives every test a fresh database with migrations applied |
| Vectors | `pgvector` crate with the `sqlx` feature | |
| Migrations | `sqlx::migrate!` over the top-level `migrations/` directory | replaces Alembic; same `NNNN_name.sql` files |
| JSON | `serde`, `serde_json` | |
| LLM output validation | `jsonschema` against the schema files in `schemas/`, then `serde` into typed structs | the schema name in a packet maps to a file |
| Prompts | `serde_yaml` for loading, `minijinja` for rendering | |
| MCP | `rmcp` (official Rust MCP SDK), streamable HTTP server, stateless, mounted in axum | |
| TEI client | `reqwest` with rustls | |
| CLI | `clap` derive | one `memvault` binary: `serve`, `worker`, `planner`, `migrate`, `admin …`, `evals …` |
| Logging, metrics | `tracing`, `tracing-subscriber` (JSON), `metrics` + `metrics-exporter-prometheus` | |
| IDs, crypto | `uuid` (v7 feature), `hmac`, `sha2`, `rand` | |
| Hook binary | `mem-hook`: `ureq`, `serde_json`, no async runtime | static release build; replaces the bash scripts |

Token estimates stay `ceil(chars / 4)`.

---

## 3. Repository layout

```
MemoryVault/
├── Cargo.toml                  # workspace; all shared dependency versions in [workspace.dependencies]
├── crates/
│   ├── core/                   # memvault-core: config, db pools + user_tx (RLS), ids, auth, schemas, TEI client + fake,
│   │                           #   prompt registry, applier registry trait, invalidation, errors, AppState
│   ├── ingest/                 # memvault-ingest: sessions, events, windows, work packets, extract/summary appliers
│   ├── recall/                 # memvault-recall: candidates, fusion, rerank, budgets, rendering, context cache, recall routes
│   ├── memories/               # memvault-memories: memories + work-state routes, MCP server
│   ├── consolidate/            # memvault-consolidate: planner, consolidation appliers, revert, admin routes and CLI
│   ├── worker/                 # memvault-worker: embedding parts and jobs, worker loop, retention
│   ├── evals/                  # memvault-evals: record, redact, replay, compare
│   ├── server/                 # memvault (binary): CLI, router assembly, migrate
│   └── mem-hook/               # mem-hook (binary): Claude Code hooks + observer runner
├── migrations/                 # NNNN_name.sql, applied by sqlx
├── schemas/                    # JSON Schemas for LLM outputs (observer.v1.json, summary.v1.json, consolidate.*.v1.json)
├── prompts/                    # observer.code.yaml, summary.*.yaml, consolidate.*.yaml
├── clients/claude-code-plugin/ # .claude-plugin/plugin.json, hooks/hooks.json, install script, README
├── deploy/                     # docker/ (server, postgres), k8s/ (kustomize base + overlays/home)
└── evals/                      # recorded sessions, questions, gold facts, baseline
```

Section 8's `mem-hook` and `mem-observe` bash scripts become two subcommands of the `mem-hook` binary (`mem-hook <event>` and `mem-hook observe`). Their behavior, flags and file layout under `~/.memory/` are unchanged. The plugin's `hooks.json` calls `${CLAUDE_PLUGIN_ROOT}/bin/mem-hook`, which the install script builds or downloads.

---

## 4. Data model (final DDL)

Conventions: UUIDv7 ids generated server-side; `timestamptz` everywhere; every memory table has `space_id`, `scope_kind`, `scope_key`; soft-delete via status or `archived_at`; hard delete only through the admin path and audited.

### 4.1 Tenancy and identity

```sql
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_search;        -- ParadeDB BM25; optional, see 4.6

CREATE TABLE users (
  id uuid PRIMARY KEY, handle text UNIQUE NOT NULL, display_name text NOT NULL,
  default_space_id uuid, created_at timestamptz NOT NULL DEFAULT now(), disabled_at timestamptz);

CREATE TABLE spaces (
  id uuid PRIMARY KEY, name text NOT NULL, kind text NOT NULL CHECK (kind IN ('private','shared')),
  owner_id uuid NOT NULL REFERENCES users(id), created_at timestamptz NOT NULL DEFAULT now());

CREATE TABLE memberships (
  space_id uuid REFERENCES spaces(id) ON DELETE CASCADE, user_id uuid REFERENCES users(id) ON DELETE CASCADE,
  role text NOT NULL CHECK (role IN ('owner','writer','reader')), PRIMARY KEY (space_id, user_id));

CREATE TABLE clients (                      -- a device or an agent runtime, owned by one user
  id uuid PRIMARY KEY, user_id uuid NOT NULL REFERENCES users(id),
  kind text NOT NULL CHECK (kind IN ('claude-code','agentic-os','runner','cli')),
  device_id text NOT NULL, hostname text, platform text,
  created_at timestamptz NOT NULL DEFAULT now(), last_seen_at timestamptz,
  UNIQUE (user_id, device_id));

CREATE TABLE api_tokens (
  id uuid PRIMARY KEY, user_id uuid NOT NULL REFERENCES users(id), client_id uuid NOT NULL REFERENCES clients(id),
  prefix text NOT NULL UNIQUE,              -- first 8 chars after 'mem_', for O(1) lookup
  secret_hash bytea NOT NULL,               -- sha256(secret)
  scopes text[] NOT NULL,                   -- see 5.2
  created_at timestamptz NOT NULL DEFAULT now(), expires_at timestamptz, revoked_at timestamptz, last_used_at timestamptz);
```

### 4.2 Sessions and raw events (episodic layer 0)

```sql
CREATE TYPE scope_kind AS ENUM ('global','project','device','agent','session');

CREATE TABLE sessions (
  id uuid PRIMARY KEY, space_id uuid NOT NULL REFERENCES spaces(id), client_id uuid NOT NULL REFERENCES clients(id),
  harness text NOT NULL,                    -- 'claude-code' | 'agentic-os'
  external_session_id text NOT NULL,        -- Claude Code session_id, or the OS conversation id
  project_key text,                         -- normalized, see 8.4
  cwd text, started_at timestamptz NOT NULL DEFAULT now(), ended_at timestamptz, end_reason text,
  metadata jsonb NOT NULL DEFAULT '{}',
  UNIQUE (client_id, external_session_id));
CREATE INDEX ON sessions (space_id, project_key, started_at DESC);

CREATE TABLE events (
  id uuid PRIMARY KEY, space_id uuid NOT NULL, session_id uuid NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq int NOT NULL,
  kind text NOT NULL CHECK (kind IN ('user_prompt','assistant_message','tool_use','tool_result','tool_failure','subagent_stop','system','note')),
  payload jsonb NOT NULL,                   -- clamped, see 6.2
  idempotency_key text NOT NULL UNIQUE,
  occurred_at timestamptz NOT NULL, received_at timestamptz NOT NULL DEFAULT now(),
  token_estimate int NOT NULL DEFAULT 0,
  window_no int,                            -- assigned at ingest, see 9.1
  UNIQUE (session_id, seq));
CREATE INDEX ON events (session_id, window_no);
```

### 4.3 Observations and summaries (episodic layers 1–2)

```sql
CREATE TABLE observations (
  id uuid PRIMARY KEY, space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL, scope_key text NOT NULL,
  session_id uuid REFERENCES sessions(id), window_no int,
  type text NOT NULL,                       -- enum per mode, see 10.2
  title text NOT NULL, subtitle text, narrative text,
  facts text[] NOT NULL DEFAULT '{}', concepts text[] NOT NULL DEFAULT '{}',
  entities text[] NOT NULL DEFAULT '{}', files text[] NOT NULL DEFAULT '{}',
  importance real NOT NULL DEFAULT 0.5 CHECK (importance BETWEEN 0 AND 1),
  sensitivity text NOT NULL DEFAULT 'normal' CHECK (sensitivity IN ('normal','sensitive','secret')),
  source_event_ids uuid[] NOT NULL DEFAULT '{}',
  extracted_by jsonb NOT NULL,              -- {"runner":"laptop-x1","runner_kind":"claude-p","model":"sonnet","prompt":"observer.code@v3","packet_id":"..."}
  written_by_client_id uuid REFERENCES clients(id),  -- set for memory_remember writes
  content_hash text NOT NULL, title_norm_key text,
  token_estimate int NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now(), archived_at timestamptz,
  superseded_by uuid REFERENCES observations(id),
  search_text text GENERATED ALWAYS AS (
    coalesce(title,'')||' '||coalesce(subtitle,'')||' '||coalesce(narrative,'')||' '||array_to_string(facts,' ')) STORED,
  search_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(title,'')||' '||coalesce(narrative,'')||' '||array_to_string(facts,' '))) STORED);
CREATE INDEX ON observations (space_id, scope_kind, scope_key, created_at DESC) WHERE archived_at IS NULL;
CREATE INDEX ON observations (space_id, scope_kind, scope_key, title_norm_key);
CREATE INDEX ON observations (space_id, content_hash);
CREATE INDEX ON observations USING gin (files);
CREATE INDEX ON observations USING gin (search_tsv);
-- pg_search BM25 index (skip when extension absent):
CREATE INDEX obs_bm25 ON observations USING bm25 (id, search_text, type, scope_key) WITH (key_field = 'id');

CREATE TABLE summaries (
  id uuid PRIMARY KEY, space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL, scope_key text NOT NULL,
  grain text NOT NULL CHECK (grain IN ('prompt','session','day','week')),
  session_id uuid REFERENCES sessions(id), prompt_no int,
  period_start timestamptz, period_end timestamptz,
  request text, investigated text, learned text, completed text, next_steps text, notes text,
  extracted_by jsonb NOT NULL, token_estimate int NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now(),
  search_text text GENERATED ALWAYS AS (coalesce(request,'')||' '||coalesce(learned,'')||' '||coalesce(completed,'')||' '||coalesce(next_steps,'')) STORED);
CREATE INDEX ON summaries (space_id, scope_kind, scope_key, grain, created_at DESC);
CREATE INDEX ON summaries (session_id);
CREATE INDEX sum_bm25 ON summaries USING bm25 (id, search_text) WITH (key_field = 'id');
```

### 4.4 Semantic and procedural tiers

```sql
CREATE TABLE entities (
  id uuid PRIMARY KEY, space_id uuid NOT NULL REFERENCES spaces(id),
  kind text NOT NULL,                       -- person | project | tool | place | org | user | other
  canonical_name text NOT NULL, aliases text[] NOT NULL DEFAULT '{}', description text,
  user_id uuid REFERENCES users(id),        -- set when the entity IS a user of the system
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (space_id, kind, canonical_name));

CREATE TABLE facts (
  id uuid PRIMARY KEY, space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL, scope_key text NOT NULL,
  subject_id uuid NOT NULL REFERENCES entities(id),
  predicate text NOT NULL,                  -- dotted, lowercase: prefers.editor, timezone, works_with, decided.<topic>
  value jsonb NOT NULL, value_text text NOT NULL,
  confidence real NOT NULL DEFAULT 0.6 CHECK (confidence BETWEEN 0 AND 1),
  importance real NOT NULL DEFAULT 0.5,
  status text NOT NULL DEFAULT 'candidate' CHECK (status IN ('candidate','active','superseded','retracted')),
  conflict bool NOT NULL DEFAULT false,
  supersedes_id uuid REFERENCES facts(id), superseded_by uuid REFERENCES facts(id),
  valid_from timestamptz, valid_to timestamptz,
  first_seen_at timestamptz NOT NULL DEFAULT now(), last_confirmed_at timestamptz NOT NULL DEFAULT now(),
  confirmations int NOT NULL DEFAULT 1,
  evidence_observation_ids uuid[] NOT NULL DEFAULT '{}',
  learned_on_client_ids uuid[] NOT NULL DEFAULT '{}',   -- denormalized, refreshed by consolidation
  access_count int NOT NULL DEFAULT 0, last_accessed_at timestamptz,
  pinned bool NOT NULL DEFAULT false,
  sensitivity text NOT NULL DEFAULT 'normal' CHECK (sensitivity IN ('normal','sensitive','secret')),
  written_by_client_id uuid REFERENCES clients(id),
  extracted_by jsonb,
  search_text text GENERATED ALWAYS AS (predicate||' '||value_text) STORED);
CREATE UNIQUE INDEX facts_one_active ON facts (space_id, scope_kind, scope_key, subject_id, predicate) WHERE status = 'active';
CREATE INDEX ON facts (space_id, status) WHERE status = 'candidate';
CREATE INDEX fact_bm25 ON facts USING bm25 (id, search_text) WITH (key_field = 'id');

CREATE TABLE procedures (
  id uuid PRIMARY KEY, space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL, scope_key text NOT NULL,
  name text NOT NULL, task_class text NOT NULL, trigger_text text NOT NULL, body_md text NOT NULL,
  version int NOT NULL DEFAULT 1, status text NOT NULL DEFAULT 'active' CHECK (status IN ('active','retired')),
  supersedes_id uuid REFERENCES procedures(id),
  success_count int NOT NULL DEFAULT 0, failure_count int NOT NULL DEFAULT 0, last_used_at timestamptz,
  source_observation_ids uuid[] NOT NULL DEFAULT '{}', token_estimate int NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now(),
  search_text text GENERATED ALWAYS AS (name||' '||trigger_text||' '||body_md) STORED);
CREATE INDEX proc_bm25 ON procedures USING bm25 (id, search_text, task_class) WITH (key_field = 'id');

CREATE TABLE work_state (
  id uuid PRIMARY KEY, space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL, scope_key text NOT NULL, list_name text NOT NULL,
  fields jsonb NOT NULL, written_by_client_id uuid REFERENCES clients(id),
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX ON work_state (space_id, scope_kind, scope_key, list_name, created_at);

CREATE TABLE profiles (                     -- cached rendering of top facts per scope
  space_id uuid NOT NULL, scope_kind scope_kind NOT NULL, scope_key text NOT NULL,
  body_md text NOT NULL, token_estimate int NOT NULL, facts_version bigint NOT NULL,
  rendered_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (space_id, scope_kind, scope_key));
```

### 4.5 Embeddings

```sql
CREATE TABLE embeddings (
  space_id uuid NOT NULL,
  owner_table text NOT NULL CHECK (owner_table IN ('observations','summaries','facts','procedures','entities')),
  owner_id uuid NOT NULL, part text NOT NULL,            -- 'title' | 'narrative' | 'fact:0'.. | 'request' | 'value' | 'body' | 'trigger'
  model text NOT NULL, embedding vector(1024) NOT NULL,
  scope_kind scope_kind NOT NULL, scope_key text NOT NULL, created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (owner_table, owner_id, part, model));
CREATE INDEX emb_hnsw ON embeddings USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 128);
CREATE INDEX ON embeddings (space_id, owner_table, scope_kind, scope_key);
```

Set `hnsw.ef_search = 80` per recall transaction; with pgvector ≥ 0.8, `SET LOCAL hnsw.iterative_scan = relaxed_order` so filtered searches do not under-fill.

### 4.6 Work, jobs, audit, logs

```sql
CREATE TABLE work_packets (                 -- LLM work, leased by clients
  id uuid PRIMARY KEY, space_id uuid NOT NULL,
  kind text NOT NULL CHECK (kind IN ('extract','summarize_prompt','summarize_session','consolidate_facts','consolidate_summary','consolidate_procedures')),
  session_id uuid REFERENCES sessions(id),  -- extract/summarize: the session; owner client is preferred runner
  preferred_client_id uuid REFERENCES clients(id),
  prompt_ref text NOT NULL,                 -- 'observer.code@v3'
  model_hint text NOT NULL,                 -- 'sonnet' | 'opus'
  inputs jsonb NOT NULL,                    -- built at creation; immutable
  output_schema text NOT NULL,              -- 'observer.v1' | 'summary.v1' | 'consolidate.facts.v1' ...
  status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','leased','done','failed','cancelled')),
  attempts int NOT NULL DEFAULT 0, max_attempts int NOT NULL DEFAULT 3,
  leased_by uuid REFERENCES clients(id), lease_until timestamptz,
  available_after timestamptz NOT NULL DEFAULT now(),
  result jsonb, result_meta jsonb,          -- model actually used, cost, duration
  last_error text, idempotency_key text UNIQUE NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(), done_at timestamptz);
CREATE INDEX ON work_packets (status, available_after, preferred_client_id) WHERE status IN ('pending','leased');

CREATE TABLE jobs (                         -- server-internal
  id uuid PRIMARY KEY, type text NOT NULL,  -- embed | render_context | reindex | expire_leases | retention
  payload jsonb NOT NULL, status text NOT NULL DEFAULT 'queued', attempts int NOT NULL DEFAULT 0,
  run_after timestamptz NOT NULL DEFAULT now(), locked_by text, locked_until timestamptz,
  idempotency_key text UNIQUE, last_error text, created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX ON jobs (status, run_after) WHERE status = 'queued';

CREATE TABLE memory_audit (
  id uuid PRIMARY KEY, space_id uuid NOT NULL,
  actor text NOT NULL,                      -- 'user:<id>' | 'client:<id>' | 'packet:<id>' | 'planner'
  action text NOT NULL,                     -- fact.create|fact.confirm|fact.supersede|fact.retract|fact.promote|obs.create|obs.archive|obs.merge|proc.create|proc.revise|delete.hard
  target_table text NOT NULL, target_id uuid NOT NULL,
  before jsonb, after jsonb, reason text, model text, prompt_version text, run_id uuid,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX ON memory_audit (run_id); CREATE INDEX ON memory_audit (target_table, target_id);

CREATE TABLE retrieval_log (
  id uuid PRIMARY KEY, space_id uuid NOT NULL, client_id uuid NOT NULL, mode text NOT NULL,
  query text, scopes jsonb NOT NULL, budgets jsonb, candidates jsonb NOT NULL, selected jsonb NOT NULL,
  feedback jsonb, latency_ms int NOT NULL, reranked bool NOT NULL, created_at timestamptz NOT NULL DEFAULT now());

CREATE TABLE access_log (                   -- every read of memory content, like claude-mem's audit_log
  id uuid PRIMARY KEY, space_id uuid NOT NULL, client_id uuid NOT NULL, action text NOT NULL,
  target_ids uuid[] NOT NULL, created_at timestamptz NOT NULL DEFAULT now());
```

### 4.7 Row-level security

Applied to every table with `space_id`. The API sets `app.user_id` per transaction; the broker and planner run as a privileged role with RLS bypass.

```sql
CREATE ROLE memsvc_app NOINHERIT LOGIN;       -- API and MCP
CREATE ROLE memsvc_system NOINHERIT LOGIN BYPASSRLS;  -- broker, planner, migrations

CREATE FUNCTION app_user_id() RETURNS uuid LANGUAGE sql STABLE AS
  $$ SELECT nullif(current_setting('app.user_id', true), '')::uuid $$;

CREATE FUNCTION readable_spaces() RETURNS SETOF uuid LANGUAGE sql STABLE AS
  $$ SELECT space_id FROM memberships WHERE user_id = app_user_id() $$;
CREATE FUNCTION writable_spaces() RETURNS SETOF uuid LANGUAGE sql STABLE AS
  $$ SELECT space_id FROM memberships WHERE user_id = app_user_id() AND role IN ('owner','writer') $$;

-- template, applied by migration to: sessions, events, observations, summaries, entities, facts, procedures,
-- work_state, profiles, embeddings, work_packets, memory_audit, retrieval_log, access_log
ALTER TABLE observations ENABLE ROW LEVEL SECURITY; ALTER TABLE observations FORCE ROW LEVEL SECURITY;
CREATE POLICY sel ON observations FOR SELECT TO memsvc_app USING (space_id IN (SELECT readable_spaces()));
CREATE POLICY ins ON observations FOR INSERT TO memsvc_app WITH CHECK (space_id IN (SELECT writable_spaces()));
CREATE POLICY upd ON observations FOR UPDATE TO memsvc_app USING (space_id IN (SELECT writable_spaces()));
CREATE POLICY del ON observations FOR DELETE TO memsvc_app USING (space_id IN (SELECT writable_spaces()));
```

The `embeddings` table carries `space_id` precisely so the vector leg is filtered by the policy, not by application code.

### 4.8 Retention

| Data | Policy |
|---|---|
| `events.payload` | Full for 30 days. Then a nightly job rewrites to `{"kind","excerpt": first 500 chars}` and sets `payload_trimmed = true`. Rows are never deleted while a derived observation references them. |
| `observations` | Never deleted automatically. `archived_at` set by consolidation (Section 12.2). |
| `facts` | Never deleted automatically. `superseded` / `retracted` only. |
| `work_packets` done/failed | 90 days, then deleted (results are already applied). |
| `retrieval_log`, `access_log` | 180 days. |
| `memory_audit` | Forever. |

---

## 5. Authentication and authorization

### 5.1 Network boundary

- Ingress is reachable only on the tailnet (Tailscale Kubernetes operator `Ingress` with `ingressClassName: tailscale`), hostname `memory.<tailnet>.ts.net`, TLS from Tailscale. No public exposure in v1.
- Defense in depth: bearer tokens are still required on the tailnet. Tailscale identity headers are not relied on in v1 because Claude Code and `curl` connect directly, not through a proxy that injects them.
- Rate limit per token: 600 requests/min for `events:write` paths, 120/min for recall and MCP. Fixed window counters in Postgres (`rate_limits(subject, window_start, count)`), fail-open on DB error, 429 with `Retry-After`.

### 5.2 Tokens

Format: `mem_<prefix 8 chars>_<secret 32 chars>`, base62. Server stores `prefix` and `sha256(secret)`. Shown once at mint. Lookup by prefix, constant-time compare of hash.

| Scope | Grants |
|---|---|
| `events:write` | `POST /v1/sessions`, `POST /v1/sessions/{id}/events`, `POST /v1/sessions/{id}/end` |
| `memories:read` | recall, context, search, timeline, get, explain, work-state read, MCP read tools |
| `memories:write` | `memory_remember`, `memory_forget` (soft), `work_state_write`, feedback |
| `work:lease` | `POST /v1/work/lease`, `POST /v1/work/{id}/result` |
| `admin` | token mint/revoke, user and space management, hard delete, consolidation trigger, revert |

Token kinds minted by the admin CLI (`memsvc admin token create --user alex --client laptop-x1 --kind device`):

- **device** (Claude Code machine): `events:write memories:read memories:write work:lease`.
- **agent** (agentic OS): same as device.
- **runner** (nightly consolidation): `work:lease memories:read`.
- **admin**: `admin` plus all of the above; never installed on a client.

Expiry: device and agent tokens 365 days; runner 90 days; admin 30 days. `last_used_at` updated at most once per minute. Revocation is immediate (lookup hits the row).

### 5.3 Request pipeline

```
Authorization: Bearer mem_xxxxxxxx_...  →  parse → prefix lookup → hash compare → expiry/revocation check
  → scope check for the route → open transaction as memsvc_app
  → SET LOCAL app.user_id = '<user>'; SET LOCAL app.client_id = '<client>'
  → handler (RLS enforces space boundary)
  → access_log row for content reads (observations/facts/procedures bodies)
```

Scopes passed in a request are validated against the token's user: a `space_id` the user is not a member of yields 403 before any query. Requests without an explicit `space_id` default to the user's `default_space_id` plus, for reads, every shared space they belong to.

### 5.4 Claude Code: MCP server registration

User scope so it is present in every project on the machine. The token is referenced, not pasted:

```bash
# in ~/.memory/config (mode 0600):  MEMORY_URL=https://memory.tailnet.ts.net  MEMORY_TOKEN=mem_...
# ~/.zshrc or ~/.bashrc:  set -a; . ~/.memory/config; set +a
claude mcp add -s user -t http memory '${MEMORY_URL}/mcp' -H 'Authorization: Bearer ${MEMORY_TOKEN}'
```

Resulting `~/.claude.json` entry (verify with `claude mcp list`, which warns if a variable is unset):

```json
{ "mcpServers": { "memory": { "type": "http", "url": "${MEMORY_URL}/mcp",
  "headers": { "Authorization": "Bearer ${MEMORY_TOKEN}" } } } }
```

Rules confirmed from Claude Code docs:
- `${VAR}` and `${VAR:-default}` expand in HTTP `url` and `headers`.
- `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `AWS_BEARER_TOKEN_BEDROCK`, `HTTPS_PROXY`, `NPM_TOKEN` are blanked in MCP headers. Never reuse those names.
- Pre-approve the tools so the user is not prompted on every call, in `~/.claude/settings.json`:

```json
{ "permissions": { "allow": ["mcp__memory__*"] } }
```

Headless runs that should use memory pass `--allowedTools "mcp__memory__*"`.

### 5.5 OAuth (Phase 4, not v1)

Claude Code supports OAuth 2.0 for remote MCP with RFC 9728 protected-resource discovery, RFC 8414 server metadata, dynamic client registration or Client ID Metadata Documents, and a `http://localhost:PORT/callback` redirect. `-p` and Agent SDK runs cannot complete OAuth interactively. For a single operator the bearer token is simpler and equally secure on a tailnet. If more than one user is onboarded:

1. Serve `/.well-known/oauth-protected-resource` pointing at an authorization server.
2. Run a small AS (authorization code + PKCE, DCR) that issues tokens the same middleware validates. Tokens map to `(user, client)` exactly like `api_tokens`.
3. Return `WWW-Authenticate: Bearer resource_metadata="..."` on 401 so Claude Code triggers discovery.

The auth middleware already abstracts "token → (user, client, scopes)", so this is additive.

---

## 6. HTTP API

Base `/v1`. JSON bodies. Errors are `{"error": {"code": "...", "message": "...", "retryable": bool}}`. All ids are UUIDs. Timestamps RFC 3339.

### 6.1 Sessions

`POST /v1/sessions` (events:write)
```json
{ "harness": "claude-code", "external_session_id": "…", "project_key": "github.com/alex/homelab", "cwd": "/home/alex/homelab",
  "space_id": null, "metadata": {"model": "opus", "source": "startup"} }
→ 201 { "session_id": "…", "space_id": "…", "window_policy": {"tool_uses": 8, "idle_seconds": 90} }
```
Idempotent on `(client, external_session_id)`: a repeat returns 200 with the existing id.

`POST /v1/sessions/{id}/end` (events:write) `{ "reason": "prompt_input_exit" }` → closes window, enqueues `summarize_session`.

### 6.2 Events

`POST /v1/sessions/{id}/events` (events:write), up to 200 events, 4 MB body.
```json
{ "events": [
  { "seq": 12, "kind": "tool_use", "occurred_at": "…", "idempotency_key": "sess:12",
    "payload": { "tool_name": "Edit", "tool_input": {...}, "tool_response": {...}, "tool_use_id": "…", "duration_ms": 120, "agent_id": null } } ] }
→ 202 { "accepted": 1, "duplicates": 0, "window_no": 3, "window_closed": false }
```

Server-side clamp per string field inside `payload`: keep head 6,144 chars + `…[clamped N chars]…` + tail 2,048 chars. Whole payload hard cap 64 KB after clamp. `token_estimate = ceil(len(json)/4)`.

Window assignment at ingest (Section 9.1). When a window closes, the handler creates the `extract` packet in the same transaction and sets `window_closed: true` in the response so the hook can start the observer immediately.

### 6.3 Recall and context

`POST /v1/recall` (memories:read); an optional `as_of` (RFC 3339) recalls what was valid at that instant
```json
{ "space_ids": ["…"], "scopes": [{"kind":"global","key":""},{"kind":"project","key":"github.com/alex/homelab"},{"kind":"device","key":"laptop-x1"}],
  "query": "redeploy the memory api after the migration", "task_state": {"intent":"deploy","entities":["memory-api"]},
  "budgets": {"profile":400,"facts":300,"procedures":400,"observations":600,"summaries":300,"work_state":200},
  "expand_top_observations": 3, "rerank": true, "exclude_sensitivity": ["secret"], "format": "both" }
→ 200 { "retrieval_id": "…", "slots": { "profile": {...}, "facts": [...], "procedures": [...], "observations": [...], "summaries": [...], "work_state": [...] },
        "rendered_markdown": "…", "tokens": {"total": 1830, "by_slot": {...}}, "latency_ms": 171, "reranked": true }
```

`GET /v1/context?project_key=…&device_id=…&source=startup` (memories:read): the `SessionStart` block. Served from cache (Section 11.6), ETag on `(space, scopes, cache_version)`. Response is `text/markdown`.

`POST /v1/recall/{retrieval_id}/feedback` (memories:write) `{ "used_ids": ["…"], "helpful": true, "notes": null }`.

### 6.4 Memories

- `GET /v1/search?q=…&space_ids=…&scopes=…&types=…&since=…&until=…&files=…&as_of=…&limit=20&offset=0` → index rows `{id, table, type, title, created_at, scope, token_estimate, score}`. Requires `q` or at least one filter. `as_of` (RFC 3339) searches the memory as it stood at that instant (see `docs/deviations.md`).
- `GET /v1/timeline?anchor_id=…&before=5&after=5` → observations and summaries around an anchor in time order, same scope.
- `POST /v1/memories/get` `{ "ids": [...] }` (≤ 20) → full bodies; writes `access_log`.
- `GET /v1/memories/{id}/explain` → Section 12.5 shape.
- `GET /v1/memories/{id}/history` → `{id, table, entries[]}`: the supersession chain through the id, oldest first. Each entry has `id, table, text, status, valid_from, valid_to, supersedes_id, superseded_by, current` and, once it was replaced or retracted, `ended_at, ended_by, reason, model, run_id` from the audit log. 404 when the id is not visible.
- `POST /v1/memories` (memories:write) `{ "kind": "fact|observation|procedure", "scope": {...}, "space_id": "…", ... }`. Facts land `active`, `confidence 0.95`, `written_by_client_id` set. Observations get `extracted_by = {"runner_kind":"explicit"}`.
- `DELETE /v1/memories/{id}` → fact `retracted` / observation `archived_at` / procedure `retired`, audited. `?hard=true` requires `admin`.
- `GET /v1/observations/by-file?path=…&project_key=…&limit=15` for the Phase 4 file-context hook.

### 6.5 Work state

- `GET /v1/work-state?scope=…&list=…` → folded lists (last write per key wins).
- `POST /v1/work-state` (memories:write) `{ "scope": {...}, "list_name": "todo", "fields": {"item-3": "done"} }`.

### 6.6 Work packets

- `POST /v1/work/lease` (work:lease) `{ "session_id": "…" | null, "kinds": ["extract","summarize_prompt"], "max": 1, "lease_seconds": 600 }` → `{ "packets": [ {id, kind, prompt_ref, model_hint, inputs, output_schema, lease_until, prompt: {system, user_template}} ] }`. The server returns the rendered prompt text so the client needs no prompt files. Selection order: packets whose `preferred_client_id` is the caller, then unassigned, then packets whose lease expired. A packet for a different preferred client is only handed out after `available_after + 10 min`.
- `POST /v1/work/{id}/result` (work:lease) `{ "output": {...}, "meta": {"model": "claude-sonnet-…", "duration_ms": 8400, "cost_usd": 0.012, "runner_kind": "claude-p", "runner": "laptop-x1"} }` → `200 {"applied": {"observations": 2, "fact_candidates": 1}}` or `422 {"error": {"code": "schema_invalid", "message": "...", "retry_with": "<validation error text to append to the prompt>"}}`.
- `POST /v1/work/{id}/fail` `{ "error": "quota_exhausted" | "cli_missing" | "timeout" | "other", "detail": "…" }` → releases the lease, `attempts += 1`, `available_after = now() + backoff`.

### 6.7 Admin (admin scope)

`/v1/admin/users`, `/v1/admin/spaces`, `/v1/admin/memberships`, `/v1/admin/tokens` (mint returns the raw token once), `/v1/admin/consolidate` (plan now), `/v1/admin/consolidation/{run_id}/revert`, `/v1/admin/jobs`, `/v1/admin/packets?status=failed`, `/v1/admin/reindex`.

Also exposed as `memsvc admin …` CLI inside the API image for use via `kubectl exec`.

---

## 7. MCP server

Mounted at `/mcp`, streamable HTTP, stateless (new server instance per request; no session affinity). Same bearer middleware as REST. Tools:

| Tool | Arguments | Returns | Cap |
|---|---|---|---|
| `memory_search` | `query: string`, `types?: string[]`, `scope?: "project"\|"global"\|"all"`, `since?: string`, `files?: string[]`, `as_of?: string (RFC 3339)`, `limit?: int (≤ 30)` | Index table: id, date, type glyph, title, ~tokens. Same renderer as the SessionStart index. | ~2k tokens |
| `memory_timeline` | `anchor_id: string`, `before?: int (≤ 10)`, `after?: int (≤ 10)` | Index rows around the anchor, with the anchor marked. | ~2k tokens |
| `memory_get` | `ids: string[] (≤ 20)` | Full bodies, rendered with citation tags. | ~6k tokens, longer bodies truncated with a marker |
| `memory_explain` | `id: string` | Provenance chain (Section 12.5) rendered as markdown. | ~2k tokens |
| `memory_history` | `id: string` | Supersession chain of a fact or procedure, oldest first, with who replaced each version and why. | ~2k tokens |
| `memory_remember` | `kind: "fact"\|"note"\|"procedure"`, `content: string`, `scope?: "project"\|"global"`, `subject?: string`, `predicate?: string` | Created id and how it will be shown. | small |
| `memory_forget` | `id: string`, `reason?: string` | Confirmation. Soft only. | small |
| `work_state_write` | `list: string`, `fields: object` | Folded list after write. | small |

Each tool's `tools/list` entry sets `_meta["anthropic/maxResultSizeChars"] = 40000` to stay under Claude Code's 50k-character spill threshold. Tool descriptions are one sentence each and include the recommended order `search → timeline → get`, as claude-mem's do. Project scope is inferred from the MCP request's `project_key` header when the Claude Code plugin sets it; otherwise `scope: "all"`.

The MCP server never writes raw events and never leases packets. Those are hook paths.

---

## 8. Claude Code plugin

### 8.1 Layout

```
claude-code-plugin/
├── .claude-plugin/plugin.json      # name "memory", version
├── hooks/hooks.json
├── bin/mem-hook                    # bash; dispatch on $1
├── bin/mem-observe                 # bash; leases packets, runs `claude -p`, posts results
└── README.md                       # install: set ~/.memory/config, `claude mcp add`, permissions allow
```

Requirements on the machine: `bash`, `curl`, `jq`, `flock`, `claude`. No Node, Bun, Python or uv.

### 8.2 hooks.json

```json
{
  "hooks": {
    "SessionStart": [
      { "matcher": "startup|resume|clear|compact",
        "hooks": [ { "type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/mem-hook session-start", "timeout": 10 } ] } ],
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/mem-hook prompt", "timeout": 5 } ] } ],
    "PostToolUse": [
      { "matcher": "*", "hooks": [ { "type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/mem-hook tool", "async": true } ] } ],
    "PostToolUseFailure": [
      { "matcher": "*", "hooks": [ { "type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/mem-hook tool-failure", "async": true } ] } ],
    "Stop": [
      { "hooks": [ { "type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/mem-hook stop", "async": true } ] } ],
    "SubagentStop": [
      { "hooks": [ { "type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/mem-hook subagent-stop", "async": true } ] } ],
    "SessionEnd": [
      { "hooks": [ { "type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/mem-hook session-end", "timeout": 2 } ] } ]
  }
}
```

Only `SessionStart` and `UserPromptSubmit` are synchronous, because only they return context. Everything else is `async: true`.

### 8.3 mem-hook behavior

Common preamble, every invocation:
1. `[ -n "$MEMORY_OBSERVER" ] && exit 0` (recursion guard).
2. Source `~/.memory/config`; if `MEMORY_URL` or `MEMORY_TOKEN` missing, exit 0 silently (and once per day write a hint to stderr on `session-start`).
3. Read stdin JSON into `$IN`. Extract `session_id`, `cwd`, `hook_event_name`, `agent_id`.
4. Resolve `project_key` (8.4). If the project is in `~/.memory/exclude` (one key per line), exit 0.
5. Resolve or create the server session id: `~/.memory/sessions/<claude_session_id>` caches `{session_id, next_seq, window}`. Missing → `POST /v1/sessions` (2 s timeout) → write cache. Failure → spool and exit 0.
6. `post()` helper: `curl -sS --max-time 2 -H "Authorization: Bearer $MEMORY_TOKEN" -H "X-Memory-Device: $DEVICE_ID" …`; on any failure append the request as one line to `~/.memory/spool/$(date +%Y%m%d).jsonl` (dir mode 0700) and exit 0.

Per event:

| Hook | Event posted | Extra behavior |
|---|---|---|
| `session-start` | `system {"source": …, "model": …}` | Drain spool first (replay in order, stop at first failure, ≤ 1.5 s). Then `GET /v1/context?…&source=$source` with `--max-time 3`. On success print `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext": <block>}}` and save the block to `~/.memory/cache/<project_key>.md`. On failure print the cached file prefixed with `> memory: server unreachable, showing cached context from <mtime>`. On `resume`, post the event but return no context (claude-mem skips context on resume too). |
| `prompt` | `user_prompt {"prompt": …}` | Also `prompt_no += 1` in the session cache. Returns nothing (no context on prompt in v1). |
| `tool` | `tool_use {tool_name, tool_input, tool_response, tool_use_id, duration_ms, agent_id}` | If response `window_closed` is true, `exec mem-observe --session <id> &` (detached, `nohup`). |
| `tool-failure` | `tool_failure {tool_name, tool_input, error, is_interrupt}` | Same. |
| `stop` | `assistant_message {last_assistant_message, stop_hook_active}` | If `stop_hook_active` is true, post nothing (the main agent is in a stop-hook loop). Server closes the window and creates `extract` + `summarize_prompt` packets; hook then runs `mem-observe --session <id> &`. |
| `subagent-stop` | `subagent_stop {agent_id, agent_type, last_assistant_message}` | Observations from subagent events are tagged `entities += ['subagent:<agent_type>']` by the extractor and excluded from the SessionStart index, as claude-mem does. |
| `session-end` | `POST /v1/sessions/{id}/end {reason}` | `--max-time 1`. Then `mem-observe --session <id> --final &`. |

Hook stdin fields used, per Claude Code docs: `session_id`, `cwd`, `hook_event_name`, `tool_name`, `tool_input`, `tool_response`, `tool_use_id`, `duration_ms`, `prompt`, `source`, `last_assistant_message`, `stop_hook_active`, `agent_id`, `agent_type`, `reason`.

### 8.4 Project key

```
git -C "$cwd" remote get-url origin  →  strip scheme, user, ".git", lowercase  →  "github.com/alex/homelab"
else: git toplevel basename, else basename(cwd)       → "local/<name>"
worktrees: `git rev-parse --git-common-dir` resolves to the main repo, so worktrees share a key.
```

Sent as `project_key` on session create and as header `X-Memory-Project` on MCP requests (the plugin cannot add per-request MCP headers, so MCP infers the project from the most recent session for this device instead; the server keeps `clients.current_project_key` updated on every event batch).

### 8.5 mem-observe (the observer runner)

```
mem-observe --session <id> [--final]
  flock -n ~/.memory/observe.lock || exit 0            # one observer per machine at a time
  loop up to 3 packets:
    P=$(post /v1/work/lease {"session_id": id, "kinds": ["extract","summarize_prompt","summarize_session"], "max": 1})
    [ empty ] && break
    export MEMORY_OBSERVER=1
    OUT=$(cd "$(mktemp -d)" && timeout 180 claude -p \
            --model "$(jq -r .model_hint <<<"$P")" \
            --output-format json \
            --disallowedTools "*" \
            --no-session-persistence \
            --max-turns 1 \
            --settings '{"disableAllHooks": true}' \
            --system-prompt "$(jq -r .prompt.system <<<"$P")" \
            "$(jq -r .prompt.user <<<"$P")")
    RESULT=$(jq -r .result <<<"$OUT")                   # the model's text; must be a JSON object
    post /v1/work/<id>/result {"output": RESULT-as-json, "meta": {model, duration, cost_usd from .total_cost_usd, runner_kind: "claude-p", runner: DEVICE_ID}}
    on 422 schema_invalid: rerun once with the error appended to the user prompt and `--model opus`; then /fail
    on claude exit != 0: /fail with stderr classification (quota|cli_missing|timeout|other)
```

Why each flag:
- `--settings '{"disableAllHooks":true}'` and `MEMORY_OBSERVER=1`: hooks still run under `-p`; `--bare` would disable them but requires an API key and so cannot use the subscription.
- `cd $(mktemp -d)`: no project `CLAUDE.md` or `.mcp.json` leaks into the observer context (claude-mem uses a dedicated observer working directory for the same reason).
- `--disallowedTools "*"`: observer must not act. `--max-turns 1`: one response.
- `--no-session-persistence`: no transcript on disk for every window.
- `--output-format json`: `.result` holds the text, `.total_cost_usd` and `.session_id` the metadata.

The agentic OS implements the same loop in Python with its Agent SDK client (`memory_client.run_packets(session_id)`), with the identical recursion rule: its own tool-use events generated during a packet run are not ingested.

### 8.6 Nightly runner

A scheduled task in the agentic OS (preferred) or `cron` on any always-on machine with Claude:

```
mem-observe --all --kinds extract,summarize_prompt,summarize_session,consolidate_facts,consolidate_summary,consolidate_procedures --max 50
```

Uses a `runner` token. Consolidation packets use `model_hint: opus`.

---

## 9. Work packet protocol

### 9.1 Window assignment

On every events batch, per session, server-side:
- `window_no` starts at 1. A window closes when any holds: 8 `tool_use`/`tool_failure` events in it; the newest event is `assistant_message` (Stop); `end` is called; or the broker's sweep finds the window idle for 90 s.
- Closing a window creates `work_packets(kind='extract', idempotency_key='extract:<session>:<window>')`. An `assistant_message` additionally creates `summarize_prompt:<session>:<prompt_no>`. `end` creates `summarize_session:<session>`.
- Windows with only `user_prompt`/`system` events create no packet.

### 9.2 Packet inputs (immutable at creation)

`extract`:
```json
{ "mode": "code", "session": {"harness": "claude-code", "project_key": "…", "device": "laptop-x1", "started_at": "…"},
  "events": [ {"seq": 12, "kind": "tool_use", "at": "…", "tool": "Edit", "input": {...}, "output": {...}} ],
  "recent_titles": [ {"id": "…", "type": "decision", "title": "…"} ],       // last 20 observations in this session, any window
  "profile_md": "…",                                                          // current profile block for project scope, ≤ 400 tokens
  "known_entities": ["memory-api", "homelab", "alex"] }
```

`summarize_prompt`: the prompt text, the window observations' titles, the final assistant message.
`summarize_session`: all prompt summaries of the session.
`consolidate_*`: Section 12.

### 9.3 Lease rules

- Lease 10 min, extendable once via `POST /v1/work/{id}/heartbeat`.
- Preferred runner is the session's client for 10 min after creation; after that any `work:lease` holder in the same space.
- `attempts >= max_attempts` → `failed`; visible in `/v1/admin/packets?status=failed`; retryable from admin.
- Result application is idempotent on `packet_id`: a duplicate result post returns the first application's counts.

### 9.4 Result validation

Output is parsed as JSON. If the model wrapped it in a code fence, the fence is stripped. Validation against the pydantic schema named by `output_schema`. On failure the server returns 422 with the error text; the runner retries once (Opus) with the error appended. Server never "fixes" output heuristically beyond fence stripping.

---

## 10. Extraction

### 10.1 Prompt registry

`server/prompts/*.yaml`, loaded at start, exposed at `GET /v1/prompts/{name}` (memories:read) for debugging. Each has `name`, `version`, `system`, `user_template` (Jinja2), `output_schema`, `model_hint`. `prompt_ref = name@version`. Changing text requires a new version; packets pin the version they were built with.

Observer prompts are ported section by section from claude-mem `plugin/modes/code.json`: `system_identity`, `observer_role` (one-way recorder, never contact the observed session), `recording_focus` with good/bad examples, `skip_guidance`, `type_guidance`, `concept_guidance`, `field_guidance`. Output format changes from XML to the JSON schema below. Added sections: `recent_titles` dedup instruction ("if a title below already records this fact, skip it"), `fact_candidates` guidance, `sensitivity` guidance.

### 10.2 Modes and types

| Mode | Observation types | Fact predicates encouraged |
|---|---|---|
| `code` | `bugfix feature refactor change discovery decision security_alert security_note sensitive` (claude-mem's nine) | `project.stack.*`, `project.decided.*`, `project.gotcha.*`, `prefers.tooling.*` |
| `assistant` | `preference decision commitment event person_fact plan correction discovery sensitive` | `prefers.*`, `person.<name>.*`, `schedule.*`, `owns.*`, `dislikes.*`, `address_as` |

Concepts (both modes): `how-it-works why-it-exists what-changed problem-solution gotcha pattern trade-off`.

### 10.3 Output schema `observer.v1`

```json
{
  "type": "object", "required": ["skip", "observations", "fact_candidates"],
  "properties": {
    "skip": {"type": "boolean"}, "skip_reason": {"type": "string"},
    "observations": {"type": "array", "maxItems": 8, "items": {
      "type": "object", "required": ["type","title","facts","concepts","importance","source_event_seqs"],
      "properties": {
        "type": {"type": "string"}, "title": {"type": "string", "maxLength": 120}, "subtitle": {"type": "string", "maxLength": 200},
        "narrative": {"type": "string", "maxLength": 2000},
        "facts": {"type": "array", "maxItems": 10, "items": {"type": "string", "maxLength": 300}},
        "concepts": {"type": "array", "minItems": 1, "maxItems": 5, "items": {"enum": ["how-it-works","why-it-exists","what-changed","problem-solution","gotcha","pattern","trade-off"]}},
        "entities": {"type": "array", "items": {"type": "string"}}, "files": {"type": "array", "items": {"type": "string"}},
        "importance": {"type": "number", "minimum": 0, "maximum": 1},
        "sensitivity": {"enum": ["normal","sensitive","secret"]},
        "source_event_seqs": {"type": "array", "minItems": 1, "items": {"type": "integer"}} } } },
    "fact_candidates": {"type": "array", "maxItems": 6, "items": {
      "type": "object", "required": ["subject","predicate","value","value_text","confidence","evidence_seqs"],
      "properties": {
        "subject": {"type": "object", "required": ["kind","name"], "properties": {"kind": {"type": "string"}, "name": {"type": "string"}}},
        "predicate": {"type": "string", "pattern": "^[a-z0-9_]+(\\.[a-z0-9_]+)*$"},
        "value": {}, "value_text": {"type": "string", "maxLength": 300},
        "confidence": {"type": "number", "minimum": 0, "maximum": 1},
        "scope_hint": {"enum": ["global","project","device","agent"]},
        "valid_from": {"type": "string"}, "evidence_seqs": {"type": "array", "items": {"type": "integer"}} } } },
    "procedure_hints": {"type": "array", "items": {"type": "object", "required": ["task_class","note"], "properties": {"task_class": {"type": "string"}, "note": {"type": "string"}}}},
    "needs_escalation": {"type": "boolean"}
  }
}
```

### 10.4 Applying an extract result (server, one transaction)

1. `skip: true` → mark packet done, store `skip_reason`, no rows.
2. For each observation: compute `content_hash = sha256(type|title|facts joined)` and `title_norm_key` (lowercase, strip punctuation/emoji, collapse whitespace; NULL if empty). If a row with the same `content_hash` exists in the space within 24 h → skip, `importance = max(old, new)`. If `title_norm_key` matches a row in the same `(space, scope, platform, agent-ness)` within 24 h → skip and audit `obs.merge`. Otherwise insert with `source_event_ids` resolved from `source_event_seqs`, `scope = project` (or `device` when `files` is empty and the content is about the machine; the prompt sets `scope_hint`), `extracted_by` from packet meta. Enqueue `jobs(embed)`.
3. For each fact candidate: resolve or create `entities(space, kind, canonical_name)` (alias match first). Look up the `active` fact for `(space, scope, subject, predicate)`.
   - none → insert `candidate`.
   - same `value` (jsonb equality, or `value_text` cosine > 0.95 after embedding) → `confirmations += 1`, `last_confirmed_at = now()`, `confidence = 1 - (1-c_old)*(1-c_new*0.5)`, add evidence and `learned_on_client_ids`, audit `fact.confirm`.
   - different value → insert `candidate` with `conflict = true`, `supersedes_id = active.id`. Consolidation decides.
4. `procedure_hints` → `procedure_hints` table rows (space, task_class, note, observation_id), consumed by consolidation.
5. `needs_escalation` on a successful result → audit note only.
6. Invalidate context cache for affected `(space, scopes)` (Section 11.6).

### 10.5 Summaries

`summary.v1` output: `{request, investigated, learned, completed, next_steps, notes}`, each ≤ 600 chars, `next_steps` mandatory and phrased as current trajectory (claude-mem's instruction). Stored as `summaries(grain='prompt')`. `summarize_session` folds all prompt summaries into one `grain='session'` row. Both embedded (`request`, `learned`, `next_steps` parts).

---

## 11. Recall engine

### 11.1 Inputs

`RecallRequest` (6.3). Server resolves `space_ids` (default: readable spaces) and validates scopes. `query` is embedded once (TEI, `bge-m3`, with the model's query instruction prefix if the model wants one). Entity terms from `task_state.entities` are appended to the BM25 query with lower weight.

### 11.2 Candidate SQL per tier (observations shown; summaries, facts, procedures analogous)

```sql
WITH bm25 AS (
  SELECT id, paradedb.score(id) AS s, row_number() OVER (ORDER BY paradedb.score(id) DESC) AS r
  FROM observations
  WHERE space_id = ANY($1) AND (scope_kind, scope_key) IN (SELECT * FROM unnest($2::scope_kind[], $3::text[]))
    AND archived_at IS NULL AND sensitivity <> ALL($4)
    AND search_text @@@ $5                                     -- pg_search query string
  LIMIT 40),
vec AS (
  SELECT e.owner_id AS id, 1 - (e.embedding <=> $6) AS s, row_number() OVER (ORDER BY e.embedding <=> $6) AS r
  FROM embeddings e JOIN observations o ON o.id = e.owner_id
  WHERE e.owner_table = 'observations' AND e.space_id = ANY($1)
    AND (e.scope_kind, e.scope_key) IN (SELECT * FROM unnest($2::scope_kind[], $3::text[]))
    AND o.archived_at IS NULL AND o.sensitivity <> ALL($4)
    AND o.created_at > now() - interval '90 days'              -- recency window: observations only
  ORDER BY e.embedding <=> $6 LIMIT 60),
fused AS (
  SELECT coalesce(b.id, v.id) AS id,
         coalesce(1.0/(60+b.r), 0) + coalesce(1.0/(60+v.r), 0) AS rrf,
         b.s AS bm25, v.s AS cos
  FROM bm25 b FULL OUTER JOIN vec v ON v.id = b.id)
SELECT f.*, o.type, o.title, o.subtitle, o.importance, o.created_at, o.token_estimate, o.session_id
FROM fused f JOIN observations o ON o.id = f.id
ORDER BY rrf DESC LIMIT 40;
```

The vector CTE dedupes by `owner_id` (multiple parts) by taking the best part: wrap in `DISTINCT ON (owner_id)` ordered by distance. When `pg_search` is absent, the `bm25` CTE uses `search_tsv @@ websearch_to_tsquery('simple', $5)` with `ts_rank_cd`.

Facts: no recency window; `status = 'active'`; pinned facts are added regardless of score. Procedures: `task_class = $intent` adds `+0.3` to the final score. Summaries: `grain IN ('session','day','week')`, 180-day window on the vector leg.

### 11.3 Rerank

If `rerank: true` and the caller's budget allows: send `(query, text)` pairs for the fused top 40 across tiers to TEI `bge-reranker-v2-m3` (`/rerank`, batch size 40). Rerank score replaces `rrf` as the base. Timeout 250 ms; on timeout fall back to `rrf` and set `reranked: false`.

### 11.4 Final score

```
recency(age_days, half_life) = 0.5 ^ (age_days / half_life)          half-life: obs 30d by default, decision/bugfix 90d, summaries 60d, facts none, procedures none
score = base × (0.5 + 0.5·importance) × (0.5 + 0.5·confidence_or_1) × (0.3 + 0.7·recency_or_1) × type_boost
```

MMR with λ = 0.7 on the best-part embeddings within each tier, so paraphrases do not fill a slot.

### 11.5 Budget fill and rendering

Per slot, greedy by score while `sum(token_estimate) ≤ budget`. `token_estimate` is stored at write time with `tiktoken` `cl100k_base` (approximation for Claude; error is consistent, which is what budgets need).

Rendering (markdown, deterministic):

```
## Memory
### Profile
- Prefers pgvector over separate vector stores for personal projects [fact#a91 · 0.83 · laptop+desktop · 2026-10-02]
### Since you were here (laptop-x1, last 2026-10-20)
- 2026-10-22 desktop · "Add rerank stage to recall" → next: wire TEI reranker container
- 2026-10-23 assistant · "Weekly review of homelab tasks" → next: rotate runner token
### Open work
- todo/memory: migrations ✓ · api deploy ☐ · eval set ☐
### Relevant procedures
- **Deploy memory-api** (used 4×, last 10-18): run migrations Job first, then roll Deployment; check /health …
### Recent observations (fetch with memory_get)
| id | when | t | title | ~tok |
| a1b2 | 10-22 14:03 | ⚖ | Chose pgvector over Chroma for the server | 140 |
…
_Legend: ⚖ decision · ● bugfix · ◆ feature · ↻ refactor · ✓ change · ○ discovery · ⚠ security_
```

Citation tag format: `[kind#shortid · confidence · devices · first_seen]`; observations show `[obs#shortid · device · date]`.

### 11.6 Context cache

Key: `(space_id, sorted scopes, device_id, source)`. Stored in `context_cache(key, body_md, etag, rendered_at)`. Invalidated by `NOTIFY memory_invalidate, '<space>:<scope_kind>:<scope_key>'` from every write path; the API listens and deletes matching keys; the worker re-renders known keys after a 2 s debounce (claude-mem's `ContextCacheService` design). Health banners and the since-you-were-here section are computed live and prepended; only the index and profile portions are cached.

### 11.7 Since-you-were-here

```sql
SELECT s.id, s.started_at, c.device_id, c.kind, sm.request, sm.next_steps
FROM sessions s JOIN clients c ON c.id = s.client_id
LEFT JOIN summaries sm ON sm.session_id = s.id AND sm.grain = 'session'
WHERE s.space_id = ANY($1) AND s.project_key = $2 AND s.client_id <> $3
  AND s.started_at > (SELECT coalesce(max(started_at), now() - interval '14 days') FROM sessions WHERE client_id = $3 AND project_key = $2 AND id <> $4)
ORDER BY s.started_at DESC LIMIT 8;
```

---

## 12. Consolidation

### 12.1 Planner (CronJob, server-side, no model)

Runs per space. Produces packets and server-only actions:

1. **Fact clusters.** Candidates grouped with the active fact they conflict with, plus any active facts whose `value` embedding cosine > 0.85 to the candidate. Each cluster of ≥ 2 rows becomes a `consolidate_facts` packet (inputs: the rows with evidence titles, `profile_md`). Lone candidates older than 24 h with no conflict are auto-activated by code when `confidence ≥ 0.6`, audited `fact.create`; otherwise they wait for the next cluster.
2. **Decay and archival (code only).** `importance *= 0.5^(days_since_last_access_or_creation / half_life)` with floors: observations 0.0, facts 0.1. Observations with `importance < 0.05` and age > 60 days → `archived_at`, audited `obs.archive`.
3. **Summaries.** One `consolidate_summary` packet per `(scope, day)` with ≥ 2 session summaries; one per week on Sundays.
4. **Procedures.** `procedure_hints` grouped by `task_class` with ≥ 3 hints across ≥ 2 sessions → `consolidate_procedures` packet with the hints, matching observations, and the existing procedure if any.
5. **Profiles.** Re-render `profiles` for scopes whose facts changed since `facts_version`.
6. **Denormalization.** Refresh `facts.learned_on_client_ids` from evidence → observations → sessions → clients.

### 12.2 `consolidate.facts.v1` output

```json
{ "decisions": [
  { "action": "supersede", "target_id": "…", "by_candidate_id": "…", "valid_from": "2026-10-01", "reason": "user stated new editor on two devices" },
  { "action": "merge_into", "target_id": "…", "candidate_id": "…", "reason": "same fact, different wording" },
  { "action": "keep_both", "ids": ["…","…"], "reason": "different scopes: work laptop vs home desktop" },
  { "action": "retract", "target_id": "…", "reason": "contradicted by three later sessions" },
  { "action": "promote_scope", "target_id": "…", "to": "global", "reason": "recurs across 3 projects" },
  { "action": "set_confidence", "target_id": "…", "confidence": 0.9, "reason": "confirmed on two devices" } ] }
```

Apply rules (code): every action writes `memory_audit` with `before`/`after` and the packet's `run_id`. `supersede` sets `valid_to` on the old row and `status='superseded'`, activates the candidate with `supersedes_id`. `merge_into` adds evidence and confirmations to the target, marks the candidate `superseded` by the target. `retract` requires the reason to cite ≥ 1 evidence id or the action is rejected and logged. `promote_scope` only within the same space; cross-space moves are never automated. Confidence after merge: `1 - Π(1 - c_i)` capped 0.98, pinned facts untouched.

### 12.3 `consolidate.summary.v1` and `consolidate.procedures.v1`

Summary output mirrors `summary.v1` plus `period`. Procedures output: `{name, task_class, trigger_text, body_md, supersedes_id?, confidence}`; a revision creates a new version row and retires the old one.

### 12.4 Revert

`POST /v1/admin/consolidation/{run_id}/revert` replays `memory_audit` rows for the run in reverse, restoring `before` snapshots, and writes a `revert` audit row per change. Packets of that run are marked `cancelled`.

### 12.5 Explain

`GET /v1/memories/{id}/explain` returns the object shown in the architecture doc Section 12: the row, each evidence observation with its session (harness, project, device), event seqs, `extracted_by`, and the audit history. For observations the chain is one level shorter.

---

## 13. Embedding and rerank service

- Two TEI containers (`ghcr.io/huggingface/text-embeddings-inference:cpu-<version>`): `embed` with `BAAI/bge-m3` (dense, 1024d, `--max-batch-tokens 16384`), `rerank` with `BAAI/bge-reranker-v2-m3`.
- Parts per row: observations `title`, `narrative`, `fact:i`; summaries `request`, `learned`, `next_steps`; facts `value` (text = `value_text`); procedures `trigger`, `body`; entities `name`. Texts over 512 tokens are truncated to the first 512 (bge-m3 accepts 8k but short parts are the point).
- `jobs(embed)` batches up to 64 texts per TEI call; the worker drains with concurrency 2.
- Backfill: `memsvc admin reindex --model bge-m3` enqueues embed jobs for rows lacking vectors for that model; runs at low priority. Embeddings for an old model are dropped only after the new model's backlog is zero and `search` has been flipped to it.
- Resources: embed 2 CPU / 4 GB, rerank 2 CPU / 3 GB. Latency targets: embed one short query ≤ 25 ms, rerank 40 pairs ≤ 150 ms.

---

## 14. Kubernetes deployment

Namespace `memory`. Kustomize base plus `overlays/home`.

| Resource | Spec |
|---|---|
| `Cluster memory-pg` (CloudNativePG) | 1 instance, Postgres 17, 50 GB PVC, `postgresql.parameters: shared_buffers=1GB, maintenance_work_mem=512MB, hnsw.ef_search=80`. Image: a CNPG-compatible image with pgvector and `pg_search`. ParadeDB publishes images bundling both; if the CNPG operator rejects it, build `ghcr.io/cloudnative-pg/postgresql:17` + the `pg_search` and `pgvector` Debian packages (`deploy/docker/postgres`). Backups: `barmanObjectStore` to S3/MinIO, WAL archiving continuous, base backup nightly, retention 30 days. Two roles (`memsvc_app`, `memsvc_system`) via `managed.roles`, passwords in Secrets generated by CNPG. |
| `Job memory-migrate` | Alembic `upgrade head` as `memsvc_system`; pre-install/pre-upgrade hook; idempotent. |
| `Deployment memory-api` | 2 replicas, `uvicorn --workers 1` per pod, 500m CPU / 512 MB each, HPA 2–4 on CPU 70%. Env: `DATABASE_URL` (app role), `TEI_EMBED_URL`, `TEI_RERANK_URL`, `PROMPTS_DIR` (ConfigMap mount), `TOKEN_PEPPER` (Secret). Readiness `GET /health` runs `SELECT 1` and checks TEI reachability (degraded, not failed, when TEI is down). |
| `Deployment memory-worker` | 1 replica, same image, `memsvc worker` (embed jobs, lease expiry sweep every 30 s, idle-window sweep every 30 s, context re-render, retention). `DATABASE_URL` system role. |
| `CronJob memory-consolidate` | `0 3 * * *`, `memsvc planner`, concurrencyPolicy Forbid, same image, system role. |
| `Deployment tei-embed`, `Deployment tei-rerank` | As Section 13; `PersistentVolumeClaim` for model cache (5 GB each) so restarts do not re-download. |
| `Ingress memory` | `ingressClassName: tailscale`, host `memory`, backend `memory-api:8000`. Annotations for `proxy-body-size: 8m`. |
| `ConfigMap memory-prompts` | the `prompts/*.yaml` files; changing a prompt is a ConfigMap update plus API rollout (prompts are cached at start). |
| `Secret memory-secrets` | `TOKEN_PEPPER`, initial admin token hash. No Claude credential anywhere. |
| `NetworkPolicy` | api and worker → postgres, tei; nothing else egresses except the Tailscale ingress path. |
| `ServiceMonitor` | scrape `/metrics` on api and worker. |

Image: `python:3.12-slim`, non-root, `uv` for deps, ~180 MB. One image for api, worker, planner, admin CLI.

---

## 15. Observability

Metrics (Prometheus, prefix `memsvc_`):
- `events_ingested_total{harness}`, `events_clamped_total`, `windows_closed_total{reason}`
- `packets_created_total{kind}`, `packets_leased_total{kind,runner_kind}`, `packets_done_total{kind}`, `packets_failed_total{kind,error}`, `packet_age_seconds` (histogram at done), `packets_pending` (gauge, by kind), `oldest_pending_packet_seconds`
- `observations_created_total{mode,type}`, `observations_skipped_total{reason}`, `fact_candidates_total`, `facts_active` (gauge), `facts_conflicts_pending` (gauge)
- `recall_latency_seconds{mode,reranked}` (histogram), `recall_candidates`, `recall_selected`, `context_cache_hit_total`
- `tei_latency_seconds{service}`, `tei_errors_total`
- `tokens_injected_total{mode,slot}`, `feedback_used_total`

Alerts: `oldest_pending_packet_seconds > 86400` (nothing has extracted for a day), `packets_failed_total` rate > 5/h, `recall_latency_seconds p95 > 0.5` for 10 min, CNPG backup failed, PVC > 80%.

Logs: JSON lines with `request_id`, `user_id`, `client_id`, `space_id`; tokens never logged; payloads never logged above DEBUG.

---

## 16. Evaluation harness and tests

### 16.1 Unit and integration

- Schema validation round-trips for every `output_schema`.
- Window assignment property tests (any event sequence yields contiguous windows closed by exactly one rule).
- RLS tests: a second user's token must see zero rows from every table, including `embeddings`, via the real API path.
- Idempotency: replaying a spool file twice produces identical row counts.
- Recall determinism: same request, same snapshot, same `selected` ids.

### 16.2 Eval set

`evals/sessions/*.jsonl` recorded from real sessions in the first two weeks (events as ingested, redacted with `memsvc evals redact`). `evals/questions.yaml`: 50–100 held-out questions with expected observation or fact ids. `memsvc evals replay --prompt observer.code@v4` rebuilds a scratch space from the sessions with the given prompt, runs recall for each question, reports recall@5, first-hit rank, tokens per hit, fact precision (against hand-labelled `evals/facts_gold.yaml`), contradiction count. CI runs it on every prompt or ranking change and fails on a drop > 5% in recall@5.

### 16.3 Side-by-side with claude-mem

For two weeks both plugins stay installed. claude-mem's `CLAUDE_MEM_CONTEXT_OBSERVATIONS` is left as is; its hooks keep running. The scorer in `evals/compare.py` pulls claude-mem's SQLite and this service's Postgres for the same session ids and reports the Section 6 table of the diagram page.

---

## 17. Milestones and acceptance criteria

| Phase | Scope | Done when |
|---|---|---|
| **1. Capture and pull-mode recall** (≈ 2 weeks) | DDL 4.1–4.3, 4.5–4.7; auth 5.1–5.4; API 6.1–6.4 minus explain; MCP search/timeline/get; plugin 8.1–8.5 with `extract` and `summarize_*` packets; TEI embed; context block with index and since-you-were-here; eval recorder. | A session on the laptop produces observations within 2 min of a window closing, using only the laptop's Claude login. A new session on the desktop shows the laptop's `next_steps` at SessionStart. Spool replays after an outage with zero duplicates. `mcp__memory__*` tools work without prompts. |
| **2. Semantic tier and push mode** (≈ 2 weeks) | `entities`, `facts`, `profiles`; fact candidates in extraction; assistant-mode prompt; `/v1/recall` with budgets, rerank, citations; Python client; agentic OS in-process packet runner; `retrieval_log` and feedback; `memory_remember`, `memory_forget`, `work_state_write`. | The agentic OS injects a budgeted memory slot on every call under 300 ms p95. Facts about the user appear in the profile after confirmation on two sessions. Feedback changes `access_count`. |
| **3. Consolidation and explain** (≈ 2 weeks) | Planner, `consolidate_*` packets, apply rules, revert, nightly runner, `explain` endpoint and tool, decay and archival, day/week summaries, eval replay in CI. | A planted contradiction is resolved within one nightly run with an audit trail, and `revert` restores it. Eval replay runs in CI with a baseline committed. |
| **4. Polish** | Procedures mining, file-context hook, Rust hook binary (Windows), admin viewer UI, OAuth for additional users, KEDA scaling, export/import per space. | Second user onboarded with a private and a shared space; RLS test suite green. |

---

## 18. Risks and open questions

- **Subscription use for background runs.** The observer runs under the user's Claude Code login on the user's machine, which is what claude-mem does. If Anthropic restricts headless background use, the switch is `ANTHROPIC_API_KEY` in `~/.memory/config` and the same `claude -p` command, or the agentic OS taking all extraction through its own credentials.
- **Quota contention.** Observer runs share the plan with the interactive session. The `flock` and the 8-tool-use window cap it; if it still bites, raise the window to 16 and make `summarize_prompt` the only per-prompt call.
- **`pg_search` availability in CNPG.** Decision D1 assumes the extension installs cleanly. The `ts_rank_cd` fallback is wired, and the eval will show whether BM25 matters enough to maintain a custom Postgres image.
- **Reranker on CPU.** 150 ms is a third of the budget. If p95 slips, rerank only in push mode and for queries longer than 6 words.
- **Project key drift.** Renamed remotes or forks change the key. `memsvc admin project remap --from --to` rewrites `project_key` on sessions and `scope_key` on memories, audited, as claude-mem's remap does.
- **MCP per-request project context.** Claude Code cannot vary MCP headers per project. The device's current project is inferred from its latest event batch; two concurrent sessions in different projects on one machine will see the union until the MCP spec or Claude Code exposes roots or a session header to remote servers.
- **Token in `~/.claude.json` by reference.** `${MEMORY_TOKEN}` expansion requires the variable in the environment Claude Code starts with. GUI launches that skip the shell profile will show the literal placeholder in `claude mcp list`; document `launchctl setenv` / systemd user environment for those.
