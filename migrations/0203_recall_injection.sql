-- Lean injection mode and token stats (module recall).
--
-- user_settings: the caller's server-side defaults, one row per user (injection_mode is what
-- GET /v1/context uses when neither the request nor the client config names a mode). No space_id,
-- so memsvc_apply_space_rls does not apply: the policy is the own-row rule of context_cache.
CREATE TABLE IF NOT EXISTS user_settings (
  user_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  injection_mode text NOT NULL DEFAULT 'full' CHECK (injection_mode IN ('full', 'lean')),
  updated_at timestamptz NOT NULL DEFAULT now());
ALTER TABLE user_settings ENABLE ROW LEVEL SECURITY;
ALTER TABLE user_settings FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS own ON user_settings;
CREATE POLICY own ON user_settings FOR ALL TO memsvc_app
  USING (user_id = app_user_id()) WITH CHECK (user_id = app_user_id());

-- What each context/recall call served: tokens of the text returned, memories selected, and for
-- context calls the injection mode (budgets already holds the token budgets). Rows from before this
-- migration have NULL tokens and are left out of the stats.
ALTER TABLE retrieval_log ADD COLUMN IF NOT EXISTS tokens int;
ALTER TABLE retrieval_log ADD COLUMN IF NOT EXISTS hits int;
ALTER TABLE retrieval_log ADD COLUMN IF NOT EXISTS injection text CHECK (injection IN ('full', 'lean'));
CREATE INDEX IF NOT EXISTS retrieval_log_client_created ON retrieval_log (client_id, created_at);

-- A cached block is per injection mode (the mode is also part of cache_key); hit_count is the
-- memories it selected, so a cache hit can be logged like a render.
ALTER TABLE context_cache ADD COLUMN IF NOT EXISTS mode text NOT NULL DEFAULT 'full' CHECK (mode IN ('full', 'lean'));
ALTER TABLE context_cache ADD COLUMN IF NOT EXISTS hit_count int NOT NULL DEFAULT 0;

-- The invalidation function now also returns the mode, so the re-render job renders the block that
-- was dropped. The return type changes, hence DROP; the grants of 0900 are restated.
DROP FUNCTION IF EXISTS memsvc_recall_invalidate(uuid, text, text);
CREATE FUNCTION memsvc_recall_invalidate(p_space uuid, p_kind text, p_key text)
RETURNS TABLE (cache_key text, user_id uuid, client_id uuid, space_id uuid, project_key text, device_id text,
               source text, mode text, rendered_at timestamptz)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
#variable_conflict use_column
BEGIN
  DELETE FROM context_invalidations WHERE context_invalidations.at < now() - interval '2 hours';
  INSERT INTO context_invalidations (space_id, scope, at)
  VALUES (p_space, CASE WHEN p_kind IS NULL THEN NULL ELSE p_kind || ':' || coalesce(p_key, '') END,
          clock_timestamp());
  RETURN QUERY
  DELETE FROM context_cache c
   WHERE (p_space IS NULL OR p_space = ANY (c.space_ids))
     AND (p_kind IS NULL OR (p_kind || ':' || coalesce(p_key, '')) = ANY (c.scopes))
  RETURNING c.cache_key, c.user_id, c.client_id, c.space_id, c.project_key, c.device_id, c.source, c.mode,
            c.rendered_at;
END
$$;
REVOKE EXECUTE ON FUNCTION memsvc_recall_invalidate(uuid, text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION memsvc_recall_invalidate(uuid, text, text) TO memsvc_app, memsvc_system;
