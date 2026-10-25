-- The observation index of a full context block can be sized and limited to some types (GET /v1/context
-- rows=, types=), so a cached block is per (rows, types) as well. The re-render job renders the block
-- that was dropped, so the invalidation function returns both. Rows cached before this migration were
-- rendered with the then default of 28 index rows; they are dropped by the next invalidation or after an hour.
ALTER TABLE context_cache ADD COLUMN IF NOT EXISTS obs_rows int NOT NULL DEFAULT 12 CHECK (obs_rows BETWEEN 1 AND 30);
ALTER TABLE context_cache ADD COLUMN IF NOT EXISTS obs_types text[] NOT NULL DEFAULT '{}';

DROP FUNCTION IF EXISTS memsvc_recall_invalidate(uuid, text, text);
CREATE FUNCTION memsvc_recall_invalidate(p_space uuid, p_kind text, p_key text)
RETURNS TABLE (cache_key text, user_id uuid, client_id uuid, space_id uuid, project_key text, device_id text,
               source text, mode text, obs_rows int, obs_types text[], rendered_at timestamptz)
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
            c.obs_rows, c.obs_types, c.rendered_at;
END
$$;
REVOKE EXECUTE ON FUNCTION memsvc_recall_invalidate(uuid, text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION memsvc_recall_invalidate(uuid, text, text) TO memsvc_app, memsvc_system;
