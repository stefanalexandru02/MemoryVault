-- Spec 4.1. pgvector is required; ParadeDB pg_search is optional (BM25 leg falls back to tsvector, 11.2).
CREATE EXTENSION IF NOT EXISTS vector;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pg_search') THEN
    BEGIN
      CREATE EXTENSION IF NOT EXISTS pg_search;
    EXCEPTION WHEN others THEN
      RAISE NOTICE 'pg_search is available but could not be created: %', SQLERRM;
    END;
  END IF;
END $$;
