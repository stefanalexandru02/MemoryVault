-- Spec 4.2: sessions and raw events (episodic layer 0).
CREATE TYPE scope_kind AS ENUM ('global','project','device','agent','session');

CREATE TABLE sessions (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL REFERENCES spaces(id),
  client_id uuid NOT NULL REFERENCES clients(id),
  harness text NOT NULL,                    -- 'claude-code' | 'agentic-os'
  external_session_id text NOT NULL,
  project_key text,                         -- normalized, spec 8.4
  cwd text,
  started_at timestamptz NOT NULL DEFAULT now(),
  ended_at timestamptz,
  end_reason text,
  metadata jsonb NOT NULL DEFAULT '{}',
  UNIQUE (client_id, external_session_id));
CREATE INDEX ON sessions (space_id, project_key, started_at DESC);
CREATE INDEX ON sessions (client_id, project_key, started_at DESC);

CREATE TABLE events (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL,
  session_id uuid NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq int NOT NULL,
  kind text NOT NULL CHECK (kind IN ('user_prompt','assistant_message','tool_use','tool_result',
                                     'tool_failure','subagent_stop','system','note')),
  payload jsonb NOT NULL,                   -- clamped, spec 6.2
  payload_trimmed bool NOT NULL DEFAULT false,  -- spec 4.8 retention rewrite
  idempotency_key text NOT NULL UNIQUE,
  occurred_at timestamptz NOT NULL,
  received_at timestamptz NOT NULL DEFAULT now(),
  token_estimate int NOT NULL DEFAULT 0,
  window_no int,                            -- assigned at ingest, spec 9.1
  UNIQUE (session_id, seq));
CREATE INDEX ON events (session_id, window_no);
CREATE INDEX ON events (received_at) WHERE NOT payload_trimmed;
