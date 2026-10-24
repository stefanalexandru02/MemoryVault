-- Spec 4.5: one vector per part across all tiers. HNSW works on pgvector >= 0.5.
CREATE TABLE embeddings (
  space_id uuid NOT NULL,
  owner_table text NOT NULL CHECK (owner_table IN ('observations','summaries','facts','procedures','entities')),
  owner_id uuid NOT NULL,
  part text NOT NULL,                       -- 'title' | 'narrative' | 'fact:0'.. | 'request' | 'value' | 'body' | 'trigger' | 'name'
  model text NOT NULL,
  embedding vector(1024) NOT NULL,
  scope_kind scope_kind NOT NULL,
  scope_key text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (owner_table, owner_id, part, model));
CREATE INDEX emb_hnsw ON embeddings USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 128);
CREATE INDEX ON embeddings (space_id, owner_table, scope_kind, scope_key);
