-- Spec 4.4: semantic and procedural tiers, working state, profiles.
CREATE TABLE entities (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL REFERENCES spaces(id),
  kind text NOT NULL,                       -- person | project | tool | place | org | user | other
  canonical_name text NOT NULL,
  aliases text[] NOT NULL DEFAULT '{}',
  description text,
  user_id uuid REFERENCES users(id),        -- set when the entity IS a user of the system
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (space_id, kind, canonical_name));
CREATE INDEX ON entities USING gin (aliases);

CREATE TABLE facts (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL,
  scope_key text NOT NULL,
  subject_id uuid NOT NULL REFERENCES entities(id),
  predicate text NOT NULL,                  -- dotted, lowercase
  value jsonb NOT NULL,
  value_text text NOT NULL,
  confidence real NOT NULL DEFAULT 0.6 CHECK (confidence BETWEEN 0 AND 1),
  importance real NOT NULL DEFAULT 0.5,
  status text NOT NULL DEFAULT 'candidate' CHECK (status IN ('candidate','active','superseded','retracted')),
  conflict bool NOT NULL DEFAULT false,
  supersedes_id uuid REFERENCES facts(id),
  superseded_by uuid REFERENCES facts(id),
  valid_from timestamptz,
  valid_to timestamptz,
  first_seen_at timestamptz NOT NULL DEFAULT now(),
  last_confirmed_at timestamptz NOT NULL DEFAULT now(),
  confirmations int NOT NULL DEFAULT 1,
  evidence_observation_ids uuid[] NOT NULL DEFAULT '{}',
  learned_on_client_ids uuid[] NOT NULL DEFAULT '{}',   -- denormalized, refreshed by consolidation
  access_count int NOT NULL DEFAULT 0,
  last_accessed_at timestamptz,
  pinned bool NOT NULL DEFAULT false,
  sensitivity text NOT NULL DEFAULT 'normal' CHECK (sensitivity IN ('normal','sensitive','secret')),
  written_by_client_id uuid REFERENCES clients(id),
  extracted_by jsonb,
  search_text text GENERATED ALWAYS AS (predicate || ' ' || value_text) STORED,
  search_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', replace(predicate, '.', ' ') || ' ' || value_text)) STORED);
CREATE UNIQUE INDEX facts_one_active ON facts (space_id, scope_kind, scope_key, subject_id, predicate) WHERE status = 'active';
CREATE INDEX ON facts (space_id, status) WHERE status = 'candidate';
CREATE INDEX ON facts (space_id, scope_kind, scope_key) WHERE status = 'active';
CREATE INDEX ON facts USING gin (search_tsv);

CREATE TABLE procedures (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL,
  scope_key text NOT NULL,
  name text NOT NULL,
  task_class text NOT NULL,
  trigger_text text NOT NULL,
  body_md text NOT NULL,
  version int NOT NULL DEFAULT 1,
  status text NOT NULL DEFAULT 'active' CHECK (status IN ('active','retired')),
  supersedes_id uuid REFERENCES procedures(id),
  success_count int NOT NULL DEFAULT 0,
  failure_count int NOT NULL DEFAULT 0,
  last_used_at timestamptz,
  source_observation_ids uuid[] NOT NULL DEFAULT '{}',
  token_estimate int NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now(),
  search_text text GENERATED ALWAYS AS (name || ' ' || trigger_text || ' ' || body_md) STORED,
  search_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', name || ' ' || trigger_text || ' ' || body_md)) STORED);
CREATE INDEX ON procedures (space_id, scope_kind, scope_key, task_class) WHERE status = 'active';
CREATE INDEX ON procedures USING gin (search_tsv);

CREATE TABLE work_state (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL,
  scope_key text NOT NULL,
  list_name text NOT NULL,
  fields jsonb NOT NULL,
  written_by_client_id uuid REFERENCES clients(id),
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX ON work_state (space_id, scope_kind, scope_key, list_name, created_at);

CREATE TABLE profiles (                     -- cached rendering of top facts per scope
  space_id uuid NOT NULL,
  scope_kind scope_kind NOT NULL,
  scope_key text NOT NULL,
  body_md text NOT NULL,
  token_estimate int NOT NULL,
  facts_version bigint NOT NULL,
  rendered_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (space_id, scope_kind, scope_key));
