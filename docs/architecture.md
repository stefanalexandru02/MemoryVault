# Shared Agent Memory Service — Architecture

A Kubernetes-hosted memory service shared by Claude Code (on several machines) and a Python agentic OS. Informed by a code review of [claude-mem](https://github.com/thedotmack/claude-mem) v15 and by the agentic-OS design brief (orchestrator + context assembler + small-model-first routing).

---

## 1. What claude-mem does, and what to take from it

claude-mem is ~1,700 TypeScript files across a Claude Code plugin, a local Bun worker, a Chroma sidecar, a multi-device sync client, and a beta hosted server. The ideas below are worth keeping; the implementation is not worth forking.

### Ideas to keep

| Idea | Where it lives in claude-mem | Why it matters |
|---|---|---|
| **Hook-driven, fail-open capture.** Every lifecycle event (`SessionStart`, `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `SubagentStop`, `SessionEnd`) runs a tiny launcher that forwards stdin to a worker over HTTP and always exits 0. | `plugin/hooks/hooks.json`, `docs/public/hooks-architecture.mdx` | Memory must never slow or break the main session. Capture is async; only context injection is synchronous. |
| **Observer as a separate model call.** A background "observer" model reads tool-use transcripts and emits structured observations: `type`, `title`, `subtitle`, `facts[]`, `narrative`, `concepts[]`, `files_read`, `files_modified`. It is told to record *what was learned/built*, never what the observer is doing, and to reply `<skip_summary reason="noise"/>` for routine operations. | `plugin/modes/code.json` (prompts), `src/services/worker/agents/` | This is exactly the "one job per call, structured output, cheap model online" pattern from the agentic-OS brief. The skip rule is what keeps the store from filling with "ran ls". |
| **Per-prompt session summaries.** On `Stop`, a second structured call writes `request / investigated / learned / completed / next_steps / notes`. | `session_summaries` table | A cheap, high-value episodic record. `next_steps` is the hand-off between machines. |
| **Progressive disclosure.** `SessionStart` injects an *index* (id, time, type glyph, title, ~token cost), not the bodies. The agent fetches details with `search → timeline → get_observations`. | `docs/public/progressive-disclosure.mdx`, `src/services/worker/search/` | Roughly 10x fewer tokens than dumping bodies. Right for Claude, which decides well what to fetch. Needs a push-mode complement for the orchestrator (see below). |
| **Hybrid retrieval with fallbacks.** Filter-only queries go to SQL. Text queries go to vector search; any empty category is refilled from FTS5/BM25; vector search applies an implicit 90-day recency window, FTS does not. | `SearchOrchestrator.ts`, `HybridSearchStrategy.ts` | Sensible defaults: keyword search is the exact-hit safety net, vectors are for paraphrase. |
| **Per-part embeddings.** An observation is embedded as several documents (narrative, each fact, title), not one blob, because MiniLM reads only 256 word pieces. Same scheme on Chroma and on pgvector (`observation_embeddings(observation_id, part)`). | `ChromaSync.formatObservationDocs`, `src/storage/postgres/observation-embeddings.ts` | Short, atomic facts embed and match far better than paragraphs. |
| **Dedup in two tiers.** Tier-0 is an O(1) lookup on a normalized-title key within (project, platform, agent). Tier-1 is an IDF-weighted cosine over tokens producing *candidates* for review, never auto-merging. | `dedup-store.ts`, `src/services/dedup/` | Cheap online dedup plus deferred, reviewable merging. |
| **Outbox + idempotency everywhere.** Event row and job row are written in one transaction; the queue publish happens after commit; a reconciler re-publishes stuck rows. Every event has an `idempotency_key`. | `IngestEventsService.ts`, `observation_generation_jobs` | Retries can never duplicate memories. |
| **Work state.** An append-only list of key/value writes per project, folded "last write wins" at read time, shown at every session start until closed. | `work-state.ts` | A shared scratchpad that survives across machines without living in the repo. |
| **File-read gate.** A `PreToolUse` hook on `Read` returns the file's observation timeline instead of a whole-file read when the file is large, unchanged, and already observed. | `docs/public/file-read-gate.mdx` | A clever token saver, but an optional, late-phase feature. |
| **Audit + deletion as first-class.** Hosted server writes `audit_log` on every read, has `DELETE /v1/memories/:id` and a per-project purge. | `src/server/` | Memory about a person needs a forget button from day one. |

### What does not fit this project

- **Local-first SQLite + two-lane cloud sync.** claude-mem's `synced_at`-as-queue, per-user ordered log, cursors, quarantine and tombstones exist because each laptop is the source of truth. With an always-on personal cluster, the server *is* the source of truth and all of that disappears. Keep only a tiny offline spool on each client.
- **Memory types are coding-only.** `bugfix / feature / refactor / change / discovery / decision` describe repo work. There is no typed semantic memory (preferences, people, decisions with confidence), no procedural memory, no consolidation, no contradiction handling, no decay. Those are exactly the tiers the agentic OS needs.
- **Scoping is `project = basename(cwd)`.** The assistant needs several axes at once: global/user, project, device, agent, session.
- **Retrieval depends on the agent calling MCP tools.** Fine for Claude Code. The agentic OS's orchestrator wants memory injected deterministically into a fixed prompt slot, so retrieval must also be available as a server-side, budgeted "context pack".
- **The observer is a long-running Claude SDK conversation per session**, recycled at 400k characters. Using the machine's own Claude login is right and is kept. What changes is the shape: one stateless, windowed `claude -p` call per ~8 tool uses instead of a long-lived SDK conversation per session.
- **Operational weight.** Bun + uv + Chroma sidecar + tree-sitter CLI per machine, 60+ SQLite schema migrations, many harness adapters. On k8s the per-machine footprint should be `curl` and one MCP URL.

---

## 2. Requirements

**Functional**
- Capture from Claude Code sessions on N machines and from the agentic OS, into one store.
- Four memory tiers: working (shared scratchpad), episodic (events, observations, summaries), semantic (typed facts about user/world), procedural (how-to notes and skills).
- Two retrieval modes from one engine: *push* (server assembles a budgeted context pack) and *pull* (index + MCP tools for progressive disclosure).
- Cheap online extraction; strong offline consolidation (dedupe, merge, resolve contradictions, decay, promote, summarize).
- Explicit remember/forget; audit of every write and every automated merge; reversible consolidation.
- Multi-scope: global, project, device, agent, session.

**Non-functional**
- Single user, personal cluster. Expect ~10k events/day, ~500 observations/day, low millions of rows and vectors over years. One Postgres node is enough.
- Context assembly p95 < 300 ms (it sits on the hot path of every orchestrator call). Capture p95 < 50 ms at the client (fire-and-forget).
- Capture never blocks or breaks a session. All hooks fail open. Clients spool locally when the server is unreachable.
- Everything model-related is swappable (Claude runs on each client's own login: `claude -p` on Claude Code machines, the Agent SDK in the agentic OS; the server never calls a model) and every prompt is versioned.
- Every retrieval and routing decision is logged so thresholds are tuned from data.

**Constraints**
- Agentic OS is Python; prefer one language on the server. Claude Code side should need no runtime beyond `curl` (hooks) and an HTTP MCP URL.

---

## 3. High-level design

```
 Machine A (laptop)                 Machine B (desktop)              Agentic OS (k8s or anywhere)
 ┌──────────────────────┐          ┌──────────────────────┐          ┌───────────────────────────┐
 │ Claude Code          │          │ Claude Code          │          │ Orchestrator (state mach.) │
 │  hooks → curl ───────┼──┐       │  hooks → curl ───────┼──┐       │  ContextAssembler ─────────┼──┐
 │  MCP (http) ─────────┼──┤       │  MCP (http) ─────────┼──┤       │  after-turn ingest ────────┼──┤
 │  ~/.memory/spool     │  │       │  ~/.memory/spool     │  │       │  memory_client (python)    │  │
 └──────────────────────┘  │       └──────────────────────┘  │       └───────────────────────────┘  │
                           └──────────────┬──────────────────┴────────────────────────────────────────┘
                                          ▼  HTTPS (Tailscale / ingress), Bearer token per client
┌──────────────────────────────────────── Kubernetes namespace: memory ────────────────────────────────┐
│                                                                                                      │
│  ┌──────────────────────┐   ┌────────────────────────┐   ┌─────────────────────┐                      │
│  │ memory-api (x2)      │   │ memory-worker (x1..n)  │   │ memory-consolidate  │                      │
│  │ FastAPI, stateless   │   │ leases LLM packets     │   │ CronJob nightly +   │                      │
│  │ /v1/* REST           │   │ validates + applies    │   │ on-demand (offline) │                      │
│  │ /mcp streamable-http │   │ embeddings             │   │ plans work packets  │                      │
│  │ recall engine        │   │ summaries              │   │ merge/decay/promote │                      │
│  └──────────┬───────────┘   └───────────┬────────────┘   └──────────┬──────────┘                      │
│             │                           │                           │                                 │
│             ▼                           ▼                           ▼                                 │
│  ┌─────────────────────────────────────────────────────────────────────────────────┐                  │
│  │ Postgres 17 + pgvector + pg_search (CloudNativePG, 1 primary, WAL + backups to S3/MinIO) │                  │
│  │ events · observations · summaries · facts · entities · procedures · work_state   │                  │
│  │ embeddings (per part) · jobs (outbox) · audit · retrieval_log · api_keys         │                  │
│  └─────────────────────────────────────────────────────────────────────────────────┘                  │
│                                                                                                      │
│  ┌─────────────────────┐   ┌──────────────────────────┐   ┌────────────────────────────┐             │
│  │ embeddings svc (TEI)│   │ no model in cluster      │   │ Secrets: client tokens,    │             │
│  │ bge-m3 + reranker   │   │ clients run Claude       │   │ prompt registry (ConfigMap)│             │
│  └─────────────────────┘   └──────────────────────────┘   └────────────────────────────┘             │
└──────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Data flow, write side:** client → `POST /v1/sessions/{id}/events` (batched, idempotent) → one transaction writes `events` + `jobs(extract)` → the client that owns the session leases the packet and runs `claude -p` (or the OS's SDK) → posts schema-validated JSON → `observations` + `fact_candidates` + embeddings → on `Stop`: `jobs(summarize)` → `session_summaries`.

**Data flow, read side:** `POST /v1/recall` → scope resolution → hybrid candidate retrieval (BM25 + cosine, RRF) per tier → rerank by recency × importance × confidence → MMR diversity → fill per-slot token budgets → return JSON + rendered markdown → log to `retrieval_log`.

**Consolidation:** nightly CronJob (and scoped runs after a session ends or a scope gathers enough new observations) → work packets → a nightly runner with Claude returns merge/supersede/retract decisions written to `facts` with full `memory_audit` trail → decay/archival → weekly summaries → procedure mining → regenerate cached `profile` block per scope.

---

## 4. Data model

Postgres only. All ids are UUIDv7 (time-ordered). Every memory row carries a `scope` so one query can read "global + this project + this device".

### Scoping

```sql
-- A scope is (kind, key). Retrieval takes a list of scopes and unions them.
CREATE TYPE scope_kind AS ENUM ('global','project','device','agent','session');
-- stored on each memory row as:
--   scope_kind scope_kind NOT NULL, scope_key text NOT NULL   (global uses key '')
```

Resolution rules live in code, not the model: Claude Code on repo `foo` reads `global`, `project:foo`, `device:<host>`; the assistant reads `global`, `agent:assistant`, plus any project the current task names. Writes default to the narrowest sensible scope; consolidation may *promote* a fact from `project:*` to `global` when it recurs across projects.

### Core tables

```sql
CREATE TABLE clients (            -- a device or an agent runtime
  id uuid PRIMARY KEY, name text, kind text CHECK (kind IN ('claude-code','agentic-os','cli','other')),
  device_id text, created_at timestamptz DEFAULT now(), last_seen_at timestamptz);

CREATE TABLE api_keys (id uuid PRIMARY KEY, client_id uuid REFERENCES clients, key_hash text UNIQUE,
  scopes text[] NOT NULL,          -- {'memories:read','memories:write','admin'}
  default_scopes jsonb,            -- [{kind:'device',key:'laptop'}, ...]
  revoked_at timestamptz, expires_at timestamptz);

CREATE TABLE sessions (
  id uuid PRIMARY KEY, client_id uuid REFERENCES clients, harness text, external_session_id text,
  project text, cwd text, started_at timestamptz, ended_at timestamptz, metadata jsonb DEFAULT '{}',
  UNIQUE (client_id, external_session_id));

-- EPISODIC tier, layer 0: raw log. Append-only, retention-managed.
CREATE TABLE events (
  id uuid PRIMARY KEY, session_id uuid REFERENCES sessions ON DELETE CASCADE, seq int,
  kind text CHECK (kind IN ('user_prompt','assistant_message','tool_use','tool_result','tool_failure','system','note')),
  payload jsonb NOT NULL,          -- clamped at ingest (head/tail 8 KB per field)
  idempotency_key text UNIQUE NOT NULL, occurred_at timestamptz NOT NULL, received_at timestamptz DEFAULT now(),
  token_estimate int);

-- EPISODIC tier, layer 1: observations (claude-mem's shape, generalized).
CREATE TABLE observations (
  id uuid PRIMARY KEY, scope_kind scope_kind, scope_key text, session_id uuid REFERENCES sessions,
  type text NOT NULL,              -- decision|bugfix|feature|refactor|discovery|change|preference|event|conversation|security|sensitive
  title text NOT NULL, subtitle text, narrative text, facts text[] DEFAULT '{}', concepts text[] DEFAULT '{}',
  entities text[] DEFAULT '{}', files text[] DEFAULT '{}',
  importance real DEFAULT 0.5,     -- 0..1, set by extractor, adjusted by consolidation
  source_event_ids uuid[] NOT NULL, content_hash text, title_norm_key text,
  token_estimate int, created_at timestamptz DEFAULT now(),
  archived_at timestamptz, superseded_by uuid REFERENCES observations,
  search tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(title,'')||' '||coalesce(subtitle,'')||' '||coalesce(narrative,'')||' '||array_to_string(facts,' '))) STORED);
CREATE INDEX ON observations USING gin(search);
CREATE INDEX ON observations (scope_kind, scope_key, created_at DESC) WHERE archived_at IS NULL;
CREATE INDEX ON observations (scope_kind, scope_key, title_norm_key);

-- EPISODIC tier, layer 2: summaries at several grains.
CREATE TABLE summaries (
  id uuid PRIMARY KEY, scope_kind scope_kind, scope_key text, grain text CHECK (grain IN ('prompt','session','day','week')),
  session_id uuid, period_start timestamptz, period_end timestamptz,
  request text, investigated text, learned text, completed text, next_steps text, notes text,
  token_estimate int, created_at timestamptz DEFAULT now(),
  search tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(request,'')||' '||coalesce(learned,'')||' '||coalesce(completed,'')||' '||coalesce(next_steps,''))) STORED);

-- SEMANTIC tier: typed, bi-temporal facts. This is what gets injected most.
CREATE TABLE entities (id uuid PRIMARY KEY, kind text, canonical_name text, aliases text[] DEFAULT '{}',
  description text, scope_kind scope_kind, scope_key text, UNIQUE (kind, canonical_name));

CREATE TABLE facts (
  id uuid PRIMARY KEY, scope_kind scope_kind, scope_key text,
  subject_id uuid REFERENCES entities,       -- who/what the fact is about (user, a person, a project, a tool)
  predicate text NOT NULL,                   -- 'prefers.editor', 'timezone', 'works_with', 'decided', 'dislikes'
  value jsonb NOT NULL, value_text text NOT NULL,   -- value_text is the one-line rendering used in prompts
  confidence real NOT NULL DEFAULT 0.6,      -- 0..1
  importance real NOT NULL DEFAULT 0.5,
  status text NOT NULL DEFAULT 'candidate' CHECK (status IN ('candidate','active','superseded','retracted')),
  supersedes_id uuid REFERENCES facts, superseded_by uuid REFERENCES facts,
  valid_from timestamptz, valid_to timestamptz,          -- when true in the world
  first_seen_at timestamptz DEFAULT now(), last_confirmed_at timestamptz DEFAULT now(),  -- when we learned/re-learned it
  evidence_observation_ids uuid[] NOT NULL DEFAULT '{}', confirmations int DEFAULT 1,
  access_count int DEFAULT 0, last_accessed_at timestamptz,
  pinned bool DEFAULT false,                 -- bypasses decay and budget ranking (e.g. "address me as X")
  sensitivity text DEFAULT 'normal' CHECK (sensitivity IN ('normal','sensitive','secret')),
  search tsvector GENERATED ALWAYS AS (to_tsvector('simple', predicate||' '||value_text)) STORED);
CREATE UNIQUE INDEX facts_one_active ON facts (scope_kind, scope_key, subject_id, predicate) WHERE status='active';

-- PROCEDURAL tier.
CREATE TABLE procedures (
  id uuid PRIMARY KEY, scope_kind scope_kind, scope_key text, name text, task_class text,
  trigger_text text,                         -- "when the user asks to deploy service X"
  body_md text NOT NULL,                     -- steps, gotchas, exemplar commands
  version int DEFAULT 1, status text DEFAULT 'active',
  success_count int DEFAULT 0, failure_count int DEFAULT 0, last_used_at timestamptz,
  source_observation_ids uuid[] DEFAULT '{}', token_estimate int, created_at timestamptz DEFAULT now(),
  search tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(name,'')||' '||coalesce(trigger_text,'')||' '||body_md)) STORED);

-- WORKING tier (shared scratchpad), append-only like claude-mem's work_state.
CREATE TABLE work_state (id uuid PRIMARY KEY, scope_kind scope_kind, scope_key text, list_name text,
  fields jsonb NOT NULL, written_by uuid REFERENCES clients, created_at timestamptz DEFAULT now());

-- One vector per PART, across all tiers (claude-mem's per-part scheme).
CREATE TABLE embeddings (
  owner_table text NOT NULL, owner_id uuid NOT NULL, part text NOT NULL,   -- 'title','narrative','fact:3','value','body'
  model text NOT NULL, embedding vector(1024) NOT NULL,
  scope_kind scope_kind, scope_key text, created_at timestamptz DEFAULT now(),
  PRIMARY KEY (owner_table, owner_id, part, model));
CREATE INDEX ON embeddings USING hnsw (embedding vector_cosine_ops);
CREATE INDEX ON embeddings (scope_kind, scope_key, owner_table);
```

### Operational tables

```sql
CREATE TABLE jobs (id uuid PRIMARY KEY, type text, payload jsonb, status text DEFAULT 'queued',
  attempts int DEFAULT 0, max_attempts int DEFAULT 3, run_after timestamptz DEFAULT now(),
  locked_by text, locked_until timestamptz, idempotency_key text UNIQUE, last_error text, created_at timestamptz DEFAULT now());
CREATE INDEX ON jobs (status, run_after) WHERE status='queued';

CREATE TABLE memory_audit (id uuid PRIMARY KEY, actor text, action text,       -- 'fact.create','fact.supersede','fact.retract','obs.archive','merge'
  target_table text, target_id uuid, before jsonb, after jsonb, reason text,
  model text, prompt_version text, run_id uuid, created_at timestamptz DEFAULT now());

CREATE TABLE retrieval_log (id uuid PRIMARY KEY, client_id uuid, mode text, query text, scopes jsonb, budgets jsonb,
  candidates jsonb, selected jsonb, latency_ms int, created_at timestamptz DEFAULT now());

CREATE TABLE prompt_versions (name text, version text, body text, model_hint text, PRIMARY KEY (name, version));
```

Retention: `events.payload` is kept in full for 30 days, then reduced to `kind`, `occurred_at` and a 500-char excerpt (observations already hold the signal). Observations are never deleted automatically, only `archived_at` (dropped from the hot index). Facts are never deleted by consolidation, only `superseded`/`retracted`; `DELETE` is a human action and is audited.

---

## 5. Write path

### 5.1 Ingest

`POST /v1/sessions/{id}/events` accepts a batch of up to 200 events. Each has a client-generated `idempotency_key` (`<session>:<seq>` or a UUID). The handler clamps large fields (tool results: head 6 KB + tail 2 KB, as claude-mem clamps at 16k chars), inserts `events` rows, and in the same transaction inserts one `jobs(type='extract')` keyed by `(session_id, window_no)` so a burst of events coalesces into one job. Returns 202 in ~5 ms.

Trigger policy for extraction windows (configurable):
- a window closes every 8 tool-use events, or 90 s after the last event, or on `Stop`;
- `Stop` also enqueues `summarize(prompt)`; `SessionEnd` enqueues `summarize(session)` and `close_session`.

### 5.2 Extraction (online, on the client's own Claude login)

Stateless, and run by whoever holds Claude for that session. The server builds the packet (leased with `UPDATE ... FOR UPDATE SKIP LOCKED` and a 10-minute lease) from:
1. the observer system prompt (versioned, adapted from claude-mem's `code.json`: "record what was learned/built, not what you are doing"; skip rule; 2–5 concepts; facts are self-contained, no pronouns);
2. the window's events, rendered compactly;
3. the titles of the last 20 observations from this session (replaces claude-mem's long-running observer conversation as the dedup context);
4. the current `profile` block for the scope, so fact candidates are phrased relative to what is already known.

Output is strict JSON validated against a schema, with one retry carrying the validation error:

```json
{
  "skip": false,
  "observations": [{
    "type": "decision", "title": "Memory service will use pgvector, not Chroma",
    "subtitle": "Single store keeps writes transactional",
    "facts": ["Embeddings are stored per part in Postgres pgvector", "Chroma sidecar is not deployed"],
    "narrative": "...", "concepts": ["trade-off","why-it-exists"], "entities": ["memory-service"],
    "files": ["k8s/postgres.yaml"], "importance": 0.8, "source_event_seqs": [12,13,15]
  }],
  "fact_candidates": [{
    "subject": {"kind":"user","name":"me"}, "predicate": "prefers.vector_store", "value": "pgvector",
    "value_text": "Prefers pgvector over separate vector databases for personal projects",
    "confidence": 0.7, "scope_hint": "global", "evidence_seqs": [13]
  }],
  "procedure_hints": [{"task_class": "deploy", "note": "Run migrations before rolling the api Deployment"}],
  "needs_escalation": false
}
```

Post-processing (plain code, on the server when the result is posted):
- Tier-0 dedup: `content_hash` and `title_norm_key` within scope and 24 h → drop or bump `importance` of the existing row.
- Embed parts (`title`, `narrative`, each `fact:i`) via the embeddings service; write `embeddings`.
- Insert `facts` with `status='candidate'`. If an `active` fact exists for the same `(scope, subject, predicate)`: identical value → `confirmations+1`, `last_confirmed_at=now()`, confidence nudged up; different value → leave as candidate flagged `conflict=true` for consolidation. The online extractor never overwrites an active fact; only consolidation does.
- `needs_escalation=true` or two schema failures → the runner retries once with `--model opus`; if it still fails, the window stays `extract_pending` for the nightly runner.

**Who runs the model.** The server never calls a model and stores no Claude credential. Every LLM call runs on a client that already has Claude, using the subscription it already pays for:

- **Claude Code machines** use the regular Claude Code login through the `claude` CLI that is already installed. When a window closes (8 tool uses, 90 s idle, or `Stop`), the async hook leases that window's packet (`POST /v1/work/lease {"session_id": ...}`), pipes the packet's prompt and inputs into `claude -p --output-format json --model sonnet`, and posts the JSON to `POST /v1/work/{id}/result`. The child run sets `MEM_OBSERVER=1` and the hook script exits immediately when it sees that variable, so the observer never triggers hooks recursively. Tools are disabled and the run starts in a scratch directory, as claude-mem does in `src/sdk/hardened-options.ts`.
- **The agentic OS** already runs Claude in-process through the Agent SDK. After each turn it leases its own packet and makes the same call with the assistant-mode prompt. No second credential.
- **Consolidation and orphan windows** (sessions whose machine slept or ran out of quota before the observer ran) are leased by a nightly runner on Opus: a scheduled task in the agentic OS, or `claude -p` on a cron on any always-on machine.

Why this shape: one prompt registry and one validation path on the server, all model spend on logins you already have, no subscription token in a cluster Secret, and capture and recall keep working even when no Claude is reachable.

Cost: about one call per 8 tool uses plus one per prompt, 5–8x fewer than claude-mem's per-tool-use observer, drawn from the same plan as the interactive session. The hook runs at most one observer at a time per machine so it never competes with the session it is watching.

In-session option: the main Claude Code agent can also call `memory_remember` for decisions it knows matter. That spends main-context tokens and the agent will not do it consistently, so it complements the background observer instead of replacing it. claude-mem keeps its observer out of the main context for the same reason.

### 5.3 Explicit writes

`POST /v1/memories` lets a client or the user write a fact, observation, or procedure directly (`memory_remember` MCP tool; the assistant's "remember that…" path). Explicit facts start `active` with `confidence=0.95`. `DELETE /v1/memories/{id}` retracts (facts) or archives (observations) and audits; `?hard=true` deletes the row and its embeddings.

### 5.4 Consolidation (offline, planned by the server, run on Claude by a runner)

A CronJob at 03:00 plus scoped runs that react to activity, as Letta's sleep-time agent does: the end of a session (once its extract packets have settled) or `CONSOLIDATE_OBSERVATION_THRESHOLD` new observations in a scope queue a `plan` job for that scope, debounced to one waiting job per scope and one run per scope per `CONSOLIDATE_MIN_INTERVAL_S`; the nightly pass skips scopes planned inside that interval. Every run records its trigger (`nightly`, `session_end`, `threshold`, `manual`) in `planner_runs`. A trigger on the candidate backlog (more than 50) or a conflict older than 24 h is not built. The server plans the work (clusters, decay set, summary periods) as packets; the nightly runner leases them, runs Opus, and posts decisions back. The server validates and applies. Runs per scope, in stages, each writing `memory_audit` rows with `run_id` so a run can be reversed:

1. **Fact resolution.** Cluster candidates with existing facts by `(subject, predicate)` and by cosine similarity > 0.85 on `value` parts. For each cluster the Opus-class model returns one of `merge_into(id)`, `supersede(id, new_value, valid_from)`, `keep_both`, `retract(id, reason)`, `promote_scope(global)`, with a justification. Code applies the decision; the model never writes SQL. Confidence is recomputed: `conf = 1 - Π(1 - c_i)` over independent confirmations, capped at 0.98 unless pinned.
2. **Decay and archival.** `importance *= 2^(-age_days / half_life[tier,type])` with half-lives of 14 days for `change/discovery` observations, 90 days for `decision/bugfix`, 365 days for facts; each access resets part of the decay (`+0.1`, capped). Observations below 0.05 importance and older than 60 days are `archived_at`. Facts never decay below 0.1 and are never archived; they are only superseded.
3. **Summaries.** Write `day` summaries from the day's `prompt`/`session` summaries; on Sundays write a `week` summary. These are what long-range recall lands on first.
4. **Procedure mining.** Group `procedure_hints` and successful task sequences by `task_class`; when the same pattern recurs ≥3 times, draft or revise a `procedures` row (versioned, old version kept). Mark `failure_count` when a later session's summary reports the procedure failed.
5. **Profile regeneration.** Render the top pinned and highest `importance × confidence` active facts per scope into a cached `profile` block (≤ 400 tokens), stored in a `profiles` table with its `facts_version`. The recall engine serves this cached block, like claude-mem's precomputed `SessionStart` cache, and invalidates it on any fact write in that scope.

---

## 6. Read path

One engine, two faces.

### 6.1 `POST /v1/recall` (push mode, for the agentic OS)

```json
{
  "scopes": [{"kind":"global"},{"kind":"agent","key":"assistant"},{"kind":"project","key":"homelab"}],
  "query": "user asked: can you redeploy the memory api after the migration",
  "task_state": {"intent":"deploy","entities":["memory-api"]},
  "budgets": {"profile": 400, "facts": 300, "procedures": 400, "observations": 600, "summaries": 300, "work_state": 200},
  "expand_top_observations": 3,
  "exclude_sensitivity": ["secret"]
}
```

Pipeline, all deterministic code:
1. **Profile slot:** cached block for the first scope that has one. Pinned facts always included.
2. **Candidates per tier (parallel SQL):** BM25 over the text columns through ParadeDB's `pg_search` index (the generated `tsvector` with `ts_rank_cd` is the fallback when the extension is unavailable), and cosine top-k over `embeddings` for the query (and for each `task_state.entities` term). Fuse with Reciprocal Rank Fusion (`k=60`). Vector search applies a 90-day default window for observations only; facts and procedures have no window; FTS has no window (claude-mem's rule, kept because exact keyword hits older than the window are the ones worth surfacing).
3. **Rerank:** the fused top 40 pass through a small cross-encoder (`bge-reranker-v2-m3` on the same TEI deployment, ~100–150 ms on CPU) when the caller's latency budget allows; then `score = rerank × (0.5 + 0.5·importance) × (0.5 + 0.5·confidence) × recency(age, half_life)`; procedures get a boost when `task_class` matches `task_state.intent`. claude-mem has no reranking stage; this is where paraphrased queries gain the most.
4. **Diversity:** MMR with λ=0.7 over embeddings so five restatements of one fact do not fill the slot.
5. **Budget fill:** greedy by score within each slot's token budget using stored `token_estimate`. Observations are rendered as an index line (`id · date · type · title · ~tokens`) and the top `expand_top_observations` also as bodies.
6. **Return** `{slots: {...}, rendered_markdown, retrieval_id}` and write `retrieval_log`. The orchestrator drops `rendered_markdown` into the memory slot of its prompt and puts the instruction last, as the brief specifies.
7. **Feedback:** `POST /v1/recall/{retrieval_id}/feedback {used_ids, helpful}` bumps `access_count` and is the signal for tuning weights.

Latency budget: profile from cache (~1 ms), two SQL legs per tier in parallel (~20–60 ms each on HNSW + GIN at this scale), one embedding call for the query (~15 ms on CPU TEI for a short string), optional rerank (~120 ms). Target p95 under 300 ms with rerank, under 150 ms without.

### 6.2 Pull mode (Claude Code)

- `SessionStart` hook → `GET /v1/context?scopes=…&device=…&format=markdown` returns a ~1k-token block: profile, open `work_state`, a **since-you-were-here** section (sessions run on *other* devices or by the assistant since this device's last session on this project, one line each: device, when, request, `next_steps`), the last 5 session summaries' `next_steps`, and an index of the top 30 observations for the project (progressive-disclosure format, with glyphs and token costs). This is `/v1/recall` with small budgets and `expand_top_observations=0`. The hand-off section is the cross-device feature claude-mem cannot render, because each device only knows its own copy until sync converges.
- The block is served from a per-scope cache invalidated on write (claude-mem's context-cache idea, server-side), so the hook returns in tens of milliseconds. The hook also writes the block to `~/.memory/cache/<project>.md` and injects that copy under a "stale, server unreachable" header when the server is down, which closes the offline-recall gap.
- MCP server at `/mcp` (streamable HTTP, stateless per request, so it needs no sticky sessions behind the ingress): `memory_search(query, scopes?, types?, since?, limit?)` → index; `memory_timeline(anchor_id, before?, after?)`; `memory_get(ids[])` → bodies; `memory_remember(kind, content, scope?)`; `memory_forget(id)`; `work_state_write(list, fields)`. Six tools, short descriptions, strict schemas.
- Optional later: `PreToolUse(Read)` → `GET /v1/observations/by-file?path=` for the file-context timeline.

---

## 7. Client adapters

### Claude Code plugin (one repo, installed on every machine)

`hooks.json` entries call one shell script with a short timeout and always exit 0, so no Node/Bun runtime is involved. Exact hooks, flags and the observer runner are in `memory-implementation-spec.md` Section 8:

```json
{ "hooks": { "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "async": true, "timeout": 10,
  "command": "${CLAUDE_PLUGIN_ROOT}/bin/mem-hook post-tool-use" }]}] } }
```

`mem-hook` is a short shell script: reads stdin JSON, adds `device_id`, `project_key` (normalized git remote, falling back to `basename(cwd)`), `external_session_id`, and `seq`; POSTs with `curl --max-time 2`; on failure appends the body to `~/.memory/spool/`. `SessionStart` drains the spool before fetching context. When the server reports a closed window, the hook starts `mem-observe` in the background, which leases the packet and runs it through `claude -p` on the machine's own Claude Code login with hooks disabled and a recursion guard. The MCP server is registered once at user scope with `claude mcp add -s user -t http memory '${MEMORY_URL}/mcp' -H 'Authorization: Bearer ${MEMORY_TOKEN}'`, and `mcp__memory__*` is pre-allowed in settings.

### Agentic OS

A small Python client (`httpx`) with `ingest(events)`, `recall(scopes, query, budgets)`, `remember(...)`, `feedback(...)`. The context assembler owns budgets per slot and calls `recall` once per model call; the orchestrator calls `ingest` after each turn with the user message, the model's structured output, and tool results. The same extraction worker serves both harnesses, with a different `mode` (observer prompt) selected by `clients.kind`: coding mode for Claude Code, "assistant" mode (preferences, people, commitments, decisions) for the OS. The OS runs on Claude through the Agent SDK, so it extracts its own turns in-process (Section 5.2) and can host the nightly runner. It can also expose the MCP tools to its agents; push mode stays the default because it is deterministic, logged, and keeps the memory slot inside a fixed token budget.

---

## 8. Kubernetes deployment

| Component | Kind | Notes |
|---|---|---|
| `postgres` | CloudNativePG `Cluster`, 1 instance, Postgres 17 image with pgvector and `pg_search` | 20–50 GB PVC; continuous WAL archiving and nightly base backup to S3/MinIO; `pg_dump` weekly as belt-and-braces. The only stateful piece. |
| `memory-api` | Deployment ×2, HPA on CPU | FastAPI + asyncpg; readiness probe runs `SELECT 1`; serves REST and `/mcp`. |
| `memory-worker` | Deployment ×1 | Python only. Hands out work packets with leases, validates and applies posted results, writes embeddings, re-queues expired leases. No model credential. |
| `memory-consolidate` | CronJob `0 3 * * *` + `kubectl create job --from` for on-demand; the worker runs the scoped `plan` jobs | Plans the Section 5.4 stages as packets; applies the decisions the runner posts back. |
| `embeddings` | Deployment, HuggingFace text-embeddings-inference, CPU | Model `bge-m3` (1024d, multilingual) or `multilingual-e5-base`. claude-mem measured a large recall gap for non-English text on MiniLM; pick multilingual from the start since re-embedding later costs hours. A second TEI container serves `bge-reranker-v2-m3` for reranking. |
| Runners (outside the cluster) | Claude Code hooks; the agentic OS; a nightly task in the OS or `claude -p` on a cron | Lease packets from `/v1/work/lease`, run Claude on their own login, post results. Nothing in the cluster stores a Claude credential. |
| Ingress | Tailscale operator or Traefik + cert-manager | Private by default; Bearer token per client; optional mTLS. |
| Secrets/Config | `Secret` for API keys and client tokens, `ConfigMap` for prompt versions and mode files | Prompt changes roll without rebuilding images. |
| Observability | Prometheus metrics (`/metrics`), Grafana dashboard: queue depth, extraction latency, skip rate, recall p95, candidates vs selected | Loki for logs if already present. |

Migrations run as an init Job (Alembic). Schema changes are additive; views isolate clients from column renames.

---

## 9. Reliability and failure modes

- **Server down:** hooks exit 0 in < 3 s; events spool locally and drain at next `SessionStart`. The agentic OS falls back to an empty memory slot and logs it. Nothing user-facing breaks.
- **Claude unavailable on a client** (quota spent, machine asleep, CLI missing): raw events are already on the server, so the window stays `extract_pending` and the nightly runner, or that machine's next session, leases it. Expired leases return to the pool. Alert when the oldest pending window is older than 24 h.
- **Bad extraction output:** schema validation + one retry + escalation to the Opus-class tier + dead-letter after `max_attempts`. Dead letters are inspectable in the admin UI.
- **Bad consolidation decision:** every change is in `memory_audit` with `run_id`; `POST /v1/admin/consolidation/{run_id}/revert` restores `before` snapshots. Candidates are never auto-deleted.
- **Duplicate deliveries:** `events.idempotency_key` and `jobs.idempotency_key` are unique; retried spools are harmless.
- **Postgres loss:** PITR from WAL archive. Embeddings are a derived projection and can be rebuilt by a `reindex` job (claude-mem's startup backfill pattern).
- **Secrets in memory:** extractor tags `sensitive` and `secret`; recall excludes `secret` by default and the `security` type is surfaced first in the index. Clients can mark a project `exclude` so no events are sent.

---

## 10. Key decisions and trade-offs

| Decision | Chosen | Alternative | Why |
|---|---|---|---|
| Source of truth | Server (Postgres on k8s) | Local-first SQLite + sync (claude-mem) | Always-on personal cluster removes the need for CRDT-style convergence. Loses offline *recall*; accepted, offline *capture* is kept via the spool. |
| Vector store | pgvector in the same Postgres | Chroma / Qdrant sidecar | One transactional store, one backup, hybrid ranking in one SQL statement. HNSW is fine to a few million vectors. Revisit if vectors exceed ~5M or if filtered recall quality degrades. |
| Queue | Postgres `jobs` table with `SKIP LOCKED` | Redis + BullMQ/arq | Fewer moving parts; outbox and job are one transaction. Revisit above ~50 jobs/s. |
| Where the model runs | On clients, on their own Claude login: `claude -p` from the hook, the Agent SDK in the OS, a nightly runner for consolidation; the server leases work and applies results | Long-lived Agent SDK conversation per laptop (claude-mem); a model worker in the cluster | Same subscription and ported prompt as claude-mem, so observation quality is equal by construction. No credential in the cluster, no extra spend, one prompt registry and one validation path. |
| Extraction granularity | Window of ~8 tool uses or 90 s, stateless | Per tool-use with a long-running observer conversation (claude-mem) | 5–8x fewer model calls, no conversation recycling. Loses some cross-window dedup context; mitigated by passing recent titles and the scope profile, which claude-mem's observer never sees. |
| Keyword ranking | ParadeDB `pg_search` BM25 | Postgres `ts_rank_cd` | Matches the FTS5 BM25 claude-mem relies on; `ts_rank_cd` is the fallback if the extension cannot be installed in the Postgres image. |
| Reranking | Cross-encoder over the fused top 40 | None (claude-mem) | Precision gain on paraphrased queries for ~120 ms on CPU; skipped when the caller's latency budget is tight. |
| Semantic memory shape | Typed `(subject, predicate, value jsonb, confidence, bi-temporal)` | Free-text "memories" with embeddings only (claude-mem, most mem0-style tools) | Enables contradiction detection by key, supersession with validity intervals, and compact rendering. Costs a stricter extraction schema. |
| Retrieval | Both push (budgeted context pack) and pull (index + MCP) from one engine | Pull only (claude-mem) | Push mode is deterministic and budgeted for the orchestrator; pull mode lets Claude choose. One engine, one retrieval log. |
| Consolidation authority | Code applies model *decisions*; model never writes directly | Agent with DB tools | Keeps every change auditable and revertable. |
| Embedding model | Multilingual (`bge-m3` or `multilingual-e5`) | `all-MiniLM-L6-v2` (claude-mem default) | Avoids the non-English recall gap claude-mem documents; re-embedding later is the expensive path. |
| Server language | Python (FastAPI) | TypeScript (reuse claude-mem code) | Matches the agentic OS; claude-mem's server is a beta coupled to its SaaS (teams, billing, better-auth). Borrow prompts and schema shapes, not code. |

---

## 11. Build order

1. **Phase 1, core capture and recall (≈2 weeks).** Postgres + pgvector, `sessions/events/observations/summaries/embeddings/jobs`, ingest, work leases, the hook-side `claude -p` observer (prompt ported from claude-mem's `code.json`, served from the prompt registry), `/v1/recall` pull-mode, `SessionStart` context, MCP `memory_search/timeline/get`, Claude Code hook script with spool. This alone replaces claude-mem across machines.
2. **Phase 2, semantic tier and push mode.** `entities/facts/profiles`, fact candidates in extraction, assistant-mode observer prompt, budgeted `/v1/recall`, Python client, `retrieval_log` and feedback.
3. **Phase 3, consolidation.** Strong-model fact resolution, decay/archival, day/week summaries, revert, admin endpoints. Add the eval harness: replay 50–100 recorded sessions and score recall@k and fact precision whenever a prompt or weight changes.
4. **Phase 4, polish.** Procedure mining, file-context hook, viewer UI, KEDA autoscaling, per-scope export/import.

**Revisit as it grows:** HNSW filtered-search quality (switch to partitioned indexes per scope or an external vector DB), reranker latency on CPU (a GPU node for TEI, or rerank only in push mode), queue throughput, and multi-user tenancy (add `tenant_id` to every table now as a constant; it costs nothing).

---

## 12. Provenance: where, what, why

claude-mem records an observation's project, time and (on synced rows) an origin device id. It does not record which machine ran the session in a queryable way for local rows, which model and prompt produced the observation, what evidence a claim rests on, or why it is still believed. The schema in Section 4 already carries the chain; this section makes the guarantees explicit.

Every memory row resolves back through foreign keys:

```
user ─owns→ client (device_id, hostname, kind)
     ─ran→ session (harness, project, cwd, started_at)
     ─captured→ events (seq, kind, payload, occurred_at)
     ─extracted by {model, prompt_version}→ observation (source_event_ids[])
     ─evidence→ fact (confidence, confirmations, valid_from/to, status, supersedes_id)
                └─ memory_audit (action, before, after, reason, model, run_id)
```

Additions to the schema from Section 4:

- `observations.extracted_by jsonb` → `{"model": "...", "prompt": "observer.code@v3", "job_id": "..."}`.
- `summaries.extracted_by jsonb`, same shape.
- `facts.evidence_observation_ids[]` is already present; add `facts.learned_on_clients uuid[]` as a denormalized cache so "learned on laptop and desktop" is one column read, refreshed by consolidation.
- Every `memory_audit` row carries `actor` (`user:<id>`, `job:extract`, `job:consolidate`), `model`, `prompt_version` and `run_id`.

**Explain endpoint.** `GET /v1/memories/{id}/explain` returns the fact, each observation it rests on with its session, client and raw event ids, the model and prompt that produced it, and the audit history. The MCP tool `memory_explain(id)` wraps it so the agent can answer "why do you think that?" from the same data a human sees.

**Citations in context packs.** Each injected fact ends with a compact tag: `[fact#a91 · 0.83 · laptop+desktop · 2026-10-02]` (id, confidence, devices it was learned on, first seen). The model can hedge on low-confidence items without being told to, and `retrieval_log` records which tags were in the prompt. `POST /v1/recall/{id}/feedback` records which were used.

## 13. Multi-user tenancy

claude-mem's database has one owner. Adding users later would mean touching every table and query, so tenancy goes into the schema now, even if one person uses it for the first year.

**Model.** `users` own `clients`. A `space` is a bucket of memory with `memberships(space_id, user_id, role in owner|writer|reader)`. Every memory table, and `embeddings`, gains `space_id NOT NULL`. Scope (global, project, device, agent, session) stays as it is, nested under the space. `api_keys` bind to exactly one user and one client.

```sql
CREATE TABLE users (id uuid PRIMARY KEY, handle text UNIQUE, display_name text, default_space_id uuid);
CREATE TABLE spaces (id uuid PRIMARY KEY, name text, kind text CHECK (kind IN ('private','shared')), owner_id uuid REFERENCES users);
CREATE TABLE memberships (space_id uuid REFERENCES spaces, user_id uuid REFERENCES users, role text, PRIMARY KEY (space_id, user_id));
ALTER TABLE clients ADD COLUMN user_id uuid REFERENCES users;
-- space_id on observations, summaries, facts, entities, procedures, work_state, embeddings, events, sessions

ALTER TABLE facts ENABLE ROW LEVEL SECURITY;
CREATE POLICY facts_read ON facts FOR SELECT USING (
  space_id IN (SELECT space_id FROM memberships WHERE user_id = current_setting('app.user_id')::uuid));
CREATE POLICY facts_write ON facts FOR INSERT WITH CHECK (
  space_id IN (SELECT space_id FROM memberships WHERE user_id = current_setting('app.user_id')::uuid AND role IN ('owner','writer')));
-- same two policies on every memory table and on embeddings
```

**Request flow.** The API resolves the token to `(user_id, client_id)`, runs `SET LOCAL app.user_id` on the connection, and lets Postgres enforce the boundary. The recall engine, MCP tools and vector search inherit it, so a forgotten `WHERE` clause in application code cannot leak across spaces.

**Rules.**
- Default read set for a device token: the user's private space plus every shared space they belong to. The agentic OS passes `space_id` per task.
- Facts about another person are stored with that person as `subject` (an `entities` row), in whichever space the observer is allowed to write. "What do I know about my partner" is a query, not a separate store.
- Promotion from a private space to a shared one happens only by the owner or an explicit "remember this for both of us". Consolidation never reads or writes across spaces.
- `sensitivity = secret` is excluded from every automatic context pack regardless of space.
- The first deployment creates one user and one private space and behaves exactly like a single-user system.

## 14. Proving it is better than claude-mem

Run both systems side by side for two weeks on the same machines, capturing the same sessions, with claude-mem in read-only mode as the baseline. Score:

| Dimension | Measurement | Target vs claude-mem |
|---|---|---|
| Cross-device continuity | 20 scripted hand-offs laptop → desktop; does the first context pack on the second machine contain the prior session's `next_steps` and decisions? | Available within 60 s of the first session's Stop, no sync setup. |
| Provenance coverage | Sample 50 facts + 50 observations; can the system name device, session, source events, model, prompt version? | 100% on all five. claude-mem: device (synced rows only) and session. |
| Recall quality | Replay 50–100 recorded sessions with held-out questions; recall@5 and rank of first correct hit, pull and push mode. | recall@5 ≥ claude-mem in pull mode; push mode has no baseline. |
| Fact precision, contradictions | Weekly human check of 30 active facts (true / stale / wrong); count active facts contradicting on the same (subject, predicate). | > 90% true after consolidation; zero unresolved contradictions older than 48 h. |
| Tokens per useful memory | `retrieval_log` tokens injected ÷ memories the feedback endpoint marks used. | < 300 tokens per used memory in push mode; match claude-mem's ~800-token index in pull mode. |
| Capture overhead | Hook wall time p95; sessions blocked or broken by a hook. | < 50 ms async hooks, < 300 ms SessionStart, zero blocked sessions. |
| Noise rate | Share of 50 sampled observations a human marks "not worth keeping". | < 20%, falling as skip rules and importance thresholds are tuned. |
| Operational footprint | Steps to onboard a machine; local services required; recovery after a lost device. | One script and one token; zero local services; nothing to recover. |

The eval set outlives either implementation. Record it from real sessions in the first two weeks, version it beside the prompts, and replay it whenever a prompt, weight or model changes.

## 15. Better than claude-mem: mechanism by mechanism

Honest split. The first group is better by construction, the second is better only if the eval confirms it, the third is equal and should stay equal.

**Better by construction**
- **One copy of memory, no sync.** Cross-device continuity is a property of the architecture, not a feature. The since-you-were-here block at `SessionStart` shows what ran elsewhere since this device's last visit.
- **Same observer, more context, no extra credentials.** Extraction runs on the same Claude Code subscription claude-mem uses, through the CLI already on the machine, with claude-mem's prompt ported, and additionally sees the session's recent observation titles and the scope's profile, so it duplicates less and phrases facts relative to what is already known.
- **Semantic and procedural tiers.** Typed, bi-temporal facts with confidence and supersession; versioned procedures; nightly consolidation that merges, resolves contradictions, decays and promotes. claude-mem has observations and summaries only.
- **Provenance and explainability.** Device, session, raw events, model and prompt version on every memory; `explain` endpoint; audit with before/after and per-run revert.
- **Multi-user.** Users, spaces, memberships and row-level security from the first migration.
- **Thin clients.** `curl` and one MCP URL per machine, no Bun, uv, Chroma or tree-sitter. Prompts live in one registry on the server.
- **Measurability.** `retrieval_log`, feedback, Prometheus metrics and a replayable eval set. claude-mem has none of these.

**Better if the eval confirms it**
- **Retrieval precision.** BM25 plus multilingual vectors plus a cross-encoder rerank should beat FTS5 plus MiniLM on paraphrased and non-English queries. Measure recall@5 and first-hit rank on the replay set before claiming it.
- **Push-mode context packs.** Deterministic, budgeted injection should cost fewer tokens per used memory than agent-driven search. Measure from `retrieval_log` and feedback.
- **Fact precision after consolidation.** Target over 90% true, zero unresolved contradictions older than 48 h. A bad merge policy would make this worse than having no facts at all, which is why decisions are audited and revertable.

**Equal, by design**
- Fail-open hooks, progressive-disclosure index format, the `search → timeline → get` tool set, per-part embeddings, Tier-0 dedup, work-state lists. These are copied because they are right.
- Offline capture (spool) is equal. Offline recall is slightly worse: the last cached context block instead of a live local database.
