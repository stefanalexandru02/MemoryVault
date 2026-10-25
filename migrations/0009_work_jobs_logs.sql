-- Spec 4.6: work packets, jobs, audit, logs; plus tables referenced elsewhere in the spec:
-- procedure_hints (10.4), context_cache (11.6), rate_limits (5.1).
CREATE TABLE work_packets (                 -- LLM work, leased by clients
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL,
  kind text NOT NULL CHECK (kind IN ('extract','summarize_prompt','summarize_session','consolidate_facts',
                                     'consolidate_summary','consolidate_procedures')),
  session_id uuid REFERENCES sessions(id),
  preferred_client_id uuid REFERENCES clients(id),
  prompt_ref text NOT NULL,                 -- 'observer.code@v3'
  model_hint text NOT NULL,                 -- 'sonnet' | 'opus'
  inputs jsonb NOT NULL,                    -- built at creation; immutable
  output_schema text NOT NULL,              -- 'observer.v1' | 'summary.v1' | 'consolidate.facts.v1' ...
  status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','leased','done','failed','cancelled')),
  attempts int NOT NULL DEFAULT 0,
  max_attempts int NOT NULL DEFAULT 3,
  leased_by uuid REFERENCES clients(id),
  lease_until timestamptz,
  available_after timestamptz NOT NULL DEFAULT now(),
  result jsonb,
  result_meta jsonb,                        -- model actually used, cost, duration
  last_error text,
  idempotency_key text UNIQUE NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  done_at timestamptz);
CREATE INDEX ON work_packets (status, available_after, preferred_client_id) WHERE status IN ('pending','leased');
CREATE INDEX ON work_packets (session_id) WHERE status IN ('pending','leased');

CREATE TABLE jobs (                         -- server-internal; no space_id, no RLS
  id uuid PRIMARY KEY,
  type text NOT NULL,                       -- embed | render_context | reindex | expire_leases | retention
  payload jsonb NOT NULL,
  status text NOT NULL DEFAULT 'queued',
  attempts int NOT NULL DEFAULT 0,
  run_after timestamptz NOT NULL DEFAULT now(),
  locked_by text,
  locked_until timestamptz,
  idempotency_key text UNIQUE,
  last_error text,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX ON jobs (status, run_after) WHERE status = 'queued';

CREATE TABLE memory_audit (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL,
  actor text NOT NULL,                      -- 'user:<id>' | 'client:<id>' | 'packet:<id>' | 'planner'
  action text NOT NULL,
  target_table text NOT NULL,
  target_id uuid NOT NULL,
  before jsonb, after jsonb, reason text, model text, prompt_version text, run_id uuid,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX ON memory_audit (run_id);
CREATE INDEX ON memory_audit (target_table, target_id);

CREATE TABLE retrieval_log (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL,
  client_id uuid NOT NULL,
  mode text NOT NULL,
  query text,
  scopes jsonb NOT NULL,
  budgets jsonb,
  candidates jsonb NOT NULL,
  selected jsonb NOT NULL,
  feedback jsonb,
  latency_ms int NOT NULL,
  reranked bool NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX ON retrieval_log (created_at);

CREATE TABLE access_log (                   -- every read of memory content
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL,
  client_id uuid NOT NULL,
  action text NOT NULL,
  target_ids uuid[] NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX ON access_log (created_at);

CREATE TABLE procedure_hints (              -- spec 10.4 step 4, consumed by consolidation (12.1 step 4)
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL,
  scope_key text NOT NULL,
  task_class text NOT NULL,
  note text NOT NULL,
  observation_id uuid REFERENCES observations(id),
  session_id uuid REFERENCES sessions(id),
  packet_id uuid REFERENCES work_packets(id),
  consumed_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX ON procedure_hints (space_id, task_class) WHERE consumed_at IS NULL;

CREATE TABLE context_cache (                -- spec 11.6; one row per rendered SessionStart block
  cache_key text PRIMARY KEY,               -- computed by recall.cache from (spaces, sorted scopes, device, source)
  user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  space_id uuid NOT NULL REFERENCES spaces(id),   -- the requesting identity's default space
  space_ids uuid[] NOT NULL,                -- every space the block was rendered from
  scopes text[] NOT NULL,                   -- 'kind:key' entries, e.g. 'global:', 'project:github.com/a/b'
  device_id text,
  source text,
  body_md text NOT NULL,
  etag text NOT NULL,
  token_estimate int NOT NULL DEFAULT 0,
  rendered_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX ON context_cache USING gin (scopes);
CREATE INDEX ON context_cache USING gin (space_ids);

CREATE TABLE rate_limits (                  -- spec 5.1: fixed-window counters per token
  subject text NOT NULL,
  window_start timestamptz NOT NULL,
  count int NOT NULL DEFAULT 0,
  PRIMARY KEY (subject, window_start));
CREATE INDEX ON rate_limits (window_start);
