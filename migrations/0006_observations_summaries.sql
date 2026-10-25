-- Spec 4.3: observations and summaries (episodic layers 1-2).
CREATE TABLE observations (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL,
  scope_key text NOT NULL,
  session_id uuid REFERENCES sessions(id),
  window_no int,
  type text NOT NULL,                       -- enum per mode, spec 10.2
  title text NOT NULL,
  subtitle text,
  narrative text,
  facts text[] NOT NULL DEFAULT '{}',
  concepts text[] NOT NULL DEFAULT '{}',
  entities text[] NOT NULL DEFAULT '{}',
  files text[] NOT NULL DEFAULT '{}',
  importance real NOT NULL DEFAULT 0.5 CHECK (importance BETWEEN 0 AND 1),
  sensitivity text NOT NULL DEFAULT 'normal' CHECK (sensitivity IN ('normal','sensitive','secret')),
  source_event_ids uuid[] NOT NULL DEFAULT '{}',
  extracted_by jsonb NOT NULL,
  written_by_client_id uuid REFERENCES clients(id),
  content_hash text NOT NULL,
  title_norm_key text,
  token_estimate int NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now(),
  archived_at timestamptz,
  superseded_by uuid REFERENCES observations(id),
  search_text text GENERATED ALWAYS AS (
    coalesce(title,'') || ' ' || coalesce(subtitle,'') || ' ' || coalesce(narrative,'') || ' ' || memsvc_join(facts, ' ')) STORED,
  search_tsv tsvector GENERATED ALWAYS AS (
    to_tsvector('simple', coalesce(title,'') || ' ' || coalesce(narrative,'') || ' ' || memsvc_join(facts, ' '))) STORED);
CREATE INDEX ON observations (space_id, scope_kind, scope_key, created_at DESC) WHERE archived_at IS NULL;
CREATE INDEX ON observations (space_id, scope_kind, scope_key, title_norm_key);
CREATE INDEX ON observations (space_id, content_hash);
CREATE INDEX ON observations (session_id, created_at);
CREATE INDEX ON observations USING gin (files);
CREATE INDEX ON observations USING gin (search_tsv);

CREATE TABLE summaries (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL,
  scope_key text NOT NULL,
  grain text NOT NULL CHECK (grain IN ('prompt','session','day','week')),
  session_id uuid REFERENCES sessions(id),
  prompt_no int,
  period_start timestamptz,
  period_end timestamptz,
  request text, investigated text, learned text, completed text, next_steps text, notes text,
  extracted_by jsonb NOT NULL,
  token_estimate int NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now(),
  search_text text GENERATED ALWAYS AS (
    coalesce(request,'') || ' ' || coalesce(learned,'') || ' ' || coalesce(completed,'') || ' ' || coalesce(next_steps,'')) STORED,
  -- tsvector fallback for the keyword leg when pg_search is absent (spec 11.2)
  search_tsv tsvector GENERATED ALWAYS AS (
    to_tsvector('simple', coalesce(request,'') || ' ' || coalesce(learned,'') || ' ' || coalesce(completed,'') || ' ' || coalesce(next_steps,''))) STORED);
CREATE INDEX ON summaries (space_id, scope_kind, scope_key, grain, created_at DESC);
CREATE INDEX ON summaries (session_id);
CREATE INDEX ON summaries USING gin (search_tsv);
