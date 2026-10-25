-- Documents: files (ADRs, READMEs, runbooks) ingested through POST /v1/sources and stored as searchable memory.
-- One row per chunk. A source is (space, scope, source_uri); re-ingesting it with another content_hash retires
-- the old chunks (retired_at) and inserts new ones, so a point-in-time search still sees what was current then.
CREATE TABLE documents (
  id uuid PRIMARY KEY,
  space_id uuid NOT NULL REFERENCES spaces(id),
  scope_kind scope_kind NOT NULL,
  scope_key text NOT NULL,
  source_uri text NOT NULL,
  title text NOT NULL,
  content_type text NOT NULL CHECK (content_type IN ('markdown', 'text')),
  chunk_index int NOT NULL,
  heading_path text[] NOT NULL DEFAULT '{}',      -- headings above the chunk, outermost first
  content text NOT NULL,
  content_hash text NOT NULL,                     -- hash of the whole source, the same on every chunk of it
  token_estimate int NOT NULL DEFAULT 0,
  ingested_by uuid REFERENCES clients(id),
  created_at timestamptz NOT NULL DEFAULT now(),  -- when the chunk was ingested
  retired_at timestamptz,
  search_text text GENERATED ALWAYS AS (title || ' ' || memsvc_join(heading_path, ' ') || ' ' || content) STORED,
  search_tsv tsvector GENERATED ALWAYS AS (
    to_tsvector('simple', title || ' ' || memsvc_join(heading_path, ' ') || ' ' || content)) STORED);
CREATE UNIQUE INDEX documents_live_chunk ON documents (space_id, scope_kind, scope_key, source_uri, chunk_index)
  WHERE retired_at IS NULL;
CREATE INDEX documents_source ON documents (space_id, scope_kind, scope_key, source_uri);
CREATE INDEX ON documents USING gin (search_tsv);
SELECT memsvc_apply_space_rls('documents');

ALTER TABLE embeddings DROP CONSTRAINT IF EXISTS embeddings_owner_table_check;
ALTER TABLE embeddings ADD CONSTRAINT embeddings_owner_table_check
  CHECK (owner_table IN ('observations', 'summaries', 'facts', 'procedures', 'entities', 'documents'));

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_search') THEN
    EXECUTE 'CREATE INDEX IF NOT EXISTS doc_bm25 ON documents USING bm25 (id, search_text) WITH (key_field = ''id'')';
  ELSE
    RAISE NOTICE 'pg_search not installed; skipping the documents BM25 index';
  END IF;
END $$;
