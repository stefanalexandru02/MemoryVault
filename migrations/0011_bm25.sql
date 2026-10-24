-- Spec 4.3/4.4: ParadeDB BM25 indexes, only when pg_search is installed. Without it the recall
-- keyword leg uses the generated search_tsv columns (spec 11.2).
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_search') THEN
    EXECUTE 'CREATE INDEX IF NOT EXISTS obs_bm25 ON observations USING bm25 (id, search_text, type, scope_key) WITH (key_field = ''id'')';
    EXECUTE 'CREATE INDEX IF NOT EXISTS sum_bm25 ON summaries USING bm25 (id, search_text) WITH (key_field = ''id'')';
    EXECUTE 'CREATE INDEX IF NOT EXISTS fact_bm25 ON facts USING bm25 (id, search_text) WITH (key_field = ''id'')';
    EXECUTE 'CREATE INDEX IF NOT EXISTS proc_bm25 ON procedures USING bm25 (id, search_text, task_class) WITH (key_field = ''id'')';
  ELSE
    RAISE NOTICE 'pg_search not installed; skipping BM25 indexes';
  END IF;
END $$;
