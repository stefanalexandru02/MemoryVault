-- Spec 4.1: tenancy and identity.
CREATE TABLE users (
  id uuid PRIMARY KEY,
  handle text UNIQUE NOT NULL,
  display_name text NOT NULL,
  default_space_id uuid,
  created_at timestamptz NOT NULL DEFAULT now(),
  disabled_at timestamptz);

CREATE TABLE spaces (
  id uuid PRIMARY KEY,
  name text NOT NULL,
  kind text NOT NULL CHECK (kind IN ('private','shared')),
  owner_id uuid NOT NULL REFERENCES users(id),
  created_at timestamptz NOT NULL DEFAULT now());

ALTER TABLE users ADD CONSTRAINT users_default_space_fk
  FOREIGN KEY (default_space_id) REFERENCES spaces(id) DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE memberships (
  space_id uuid REFERENCES spaces(id) ON DELETE CASCADE,
  user_id uuid REFERENCES users(id) ON DELETE CASCADE,
  role text NOT NULL CHECK (role IN ('owner','writer','reader')),
  PRIMARY KEY (space_id, user_id));
CREATE INDEX ON memberships (user_id);

CREATE TABLE clients (                      -- a device or an agent runtime, owned by one user
  id uuid PRIMARY KEY,
  user_id uuid NOT NULL REFERENCES users(id),
  kind text NOT NULL CHECK (kind IN ('claude-code','agentic-os','runner','cli')),
  device_id text NOT NULL,
  hostname text,
  platform text,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_seen_at timestamptz,
  current_project_key text,                 -- spec 8.4: latest project seen in an events batch (MCP inference)
  current_project_at timestamptz,
  UNIQUE (user_id, device_id));

CREATE TABLE api_tokens (
  id uuid PRIMARY KEY,
  user_id uuid NOT NULL REFERENCES users(id),
  client_id uuid NOT NULL REFERENCES clients(id),
  kind text NOT NULL DEFAULT 'device' CHECK (kind IN ('device','agent','runner','admin')),
  prefix text NOT NULL UNIQUE,              -- the 8 chars after 'mem_', for O(1) lookup
  secret_hash bytea NOT NULL,               -- hmac_sha256(TOKEN_PEPPER, secret)
  scopes text[] NOT NULL,                   -- see spec 5.2
  created_at timestamptz NOT NULL DEFAULT now(),
  expires_at timestamptz,
  revoked_at timestamptz,
  last_used_at timestamptz);
CREATE INDEX ON api_tokens (user_id);
