-- Memory quality signals: per-memory feedback and the queue of disputed facts.

-- One row per (user, memory, verdict): helpful | wrong | outdated, with who, when and an optional note.
-- Append-only: rows are inserted and never updated; they go away only with a hard delete of the memory.
-- The unique index makes a repeated verdict a no-op (`ON CONFLICT DO NOTHING`), so clicks do not stack.
CREATE TABLE memory_feedback (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL,
  user_id uuid NOT NULL,
  client_id uuid,
  target_table text NOT NULL CHECK (target_table IN ('facts', 'observations', 'procedures')),
  target_id uuid NOT NULL,
  verdict text NOT NULL CHECK (verdict IN ('helpful', 'wrong', 'outdated')),
  note text,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE UNIQUE INDEX memory_feedback_once ON memory_feedback (user_id, target_id, verdict);
CREATE INDEX memory_feedback_target ON memory_feedback (target_id);
SELECT memsvc_apply_space_rls('memory_feedback');

-- A fact whose net negative feedback crossed the threshold waits here for the planner, which retracts it
-- (or leaves it to a pending replacement) under a run id. NULL = not queued. Feedback sets and clears it.
ALTER TABLE facts ADD COLUMN IF NOT EXISTS disputed_at timestamptz;
CREATE INDEX IF NOT EXISTS facts_disputed ON facts (space_id) WHERE disputed_at IS NOT NULL;
