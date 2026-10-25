-- Ingest module (spec 9.1, 9.3): per-session window state, deferred summaries, lease bookkeeping.

-- One row per window of a session. The open window (closed_at IS NULL) carries the running counters;
-- the session row is the lock that serialises every change to a session's windows.
CREATE TABLE session_windows (
  space_id uuid NOT NULL REFERENCES spaces(id),
  session_id uuid NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  window_no int NOT NULL,
  prompt_no int NOT NULL DEFAULT 0,          -- user_prompt events seen in the session up to this window
  tool_uses int NOT NULL DEFAULT 0,          -- tool_use + tool_failure events in the window
  event_count int NOT NULL DEFAULT 0,
  has_content bool NOT NULL DEFAULT false,   -- any event other than user_prompt/system
  opened_at timestamptz NOT NULL DEFAULT now(),
  last_event_at timestamptz NOT NULL DEFAULT now(),
  closed_at timestamptz,
  close_reason text CHECK (close_reason IN ('tool_uses','stop','end','idle')),
  extract_packet_id uuid REFERENCES work_packets(id) ON DELETE SET NULL,
  -- closed by a Stop: a summarize_prompt packet is still owed for prompt_no (created once the
  -- prompt's extract packets have settled, so its inputs can carry their observation titles)
  prompt_summary_pending bool NOT NULL DEFAULT false,
  PRIMARY KEY (session_id, window_no));
CREATE UNIQUE INDEX session_windows_one_open ON session_windows (session_id) WHERE closed_at IS NULL;
CREATE INDEX ON session_windows (last_event_at) WHERE closed_at IS NULL;
CREATE INDEX ON session_windows (session_id, prompt_no);
CREATE INDEX ON session_windows (session_id) WHERE prompt_summary_pending;
SELECT memsvc_apply_space_rls('session_windows');

-- Set by POST /v1/sessions/{id}/end: a summarize_session packet is owed once the session's extract and
-- summarize_prompt packets have settled.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS session_summary_pending bool NOT NULL DEFAULT false;
CREATE INDEX IF NOT EXISTS sessions_summary_pending ON sessions (id) WHERE session_summary_pending;

-- Lease bookkeeping (9.3) and the counts of the first result application (idempotent replays).
ALTER TABLE work_packets ADD COLUMN IF NOT EXISTS lease_seconds int;
ALTER TABLE work_packets ADD COLUMN IF NOT EXISTS heartbeats int NOT NULL DEFAULT 0;
ALTER TABLE work_packets ADD COLUMN IF NOT EXISTS applied jsonb;
CREATE INDEX IF NOT EXISTS work_packets_session_kind ON work_packets (session_id, kind);
