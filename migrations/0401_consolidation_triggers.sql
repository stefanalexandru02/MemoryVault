-- Triggered consolidation: when a scope was last planned, the log of planner runs with the trigger that
-- started them, and at most one live scoped `plan` job per scope.

-- One row per (space, scope) the planner has processed. A triggered run waits until `last_planned_at` +
-- CONSOLIDATE_MIN_INTERVAL_S, the nightly pass leaves a scope planned inside that window alone, and the
-- observation threshold counts what arrived after it.
CREATE TABLE scope_planning (
  space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL,
  scope_key text NOT NULL,
  last_planned_at timestamptz NOT NULL,
  PRIMARY KEY (space_id, scope_kind, scope_key));
SELECT memsvc_apply_space_rls('scope_planning');

-- One row per planner run and space: what started it (nightly | session_end | threshold | manual), the
-- scope it was restricted to (NULL = every scope of the space) and what it did. `run_id` is the id the
-- run's audit rows and packets carry.
CREATE TABLE planner_runs (
  run_id uuid NOT NULL,
  space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind,
  scope_key text,
  trigger text NOT NULL CHECK (trigger IN ('nightly', 'session_end', 'threshold', 'manual')),
  packets jsonb NOT NULL DEFAULT '{}',
  actions jsonb NOT NULL DEFAULT '{}',
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (run_id, space_id));
CREATE INDEX planner_runs_created ON planner_runs (created_at DESC);
SELECT memsvc_apply_space_rls('planner_runs');

-- A scope has at most one `plan` job that is waiting or running; a trigger that finds one is a no-op.
CREATE UNIQUE INDEX jobs_plan_live ON jobs ((payload ->> 'space_id'), (payload #>> '{scope,kind}'), (payload #>> '{scope,key}'))
  WHERE type = 'plan' AND status IN ('queued', 'running');
