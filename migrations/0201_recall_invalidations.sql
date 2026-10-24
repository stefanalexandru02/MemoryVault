-- Durable record of invalidations, so a render that raced with a write is not served stale.
--
-- A render reads in several statements and commits later; a NOTIFY handled in between deletes
-- nothing (the new row is not visible yet). memsvc_recall_invalidate therefore also records
-- (space, scope, at), and a cached block is fresh only when no matching invalidation is later than
-- the block's rendered_at (taken before the render read anything).
CREATE TABLE IF NOT EXISTS context_invalidations (
  space_id uuid,          -- NULL = every space
  scope    text,          -- '<kind>:<key>'; NULL = the whole space
  at       timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX IF NOT EXISTS context_invalidations_at ON context_invalidations (at);
REVOKE ALL ON context_invalidations FROM PUBLIC;

CREATE OR REPLACE FUNCTION memsvc_recall_invalidate(p_space uuid, p_kind text, p_key text)
RETURNS TABLE (cache_key text, user_id uuid, client_id uuid, space_id uuid, project_key text, device_id text,
               source text, rendered_at timestamptz)
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
  RETURNING c.cache_key, c.user_id, c.client_id, c.space_id, c.project_key, c.device_id, c.source, c.rendered_at;
END
$$;

-- True when an invalidation matching a block's spaces and scopes happened after it began rendering.
CREATE OR REPLACE FUNCTION memsvc_context_stale(p_space_ids uuid[], p_scopes text[], p_rendered_at timestamptz)
RETURNS boolean LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp AS $$
  SELECT EXISTS (
    SELECT 1 FROM context_invalidations i
     WHERE i.at > p_rendered_at
       AND (i.space_id IS NULL OR i.space_id = ANY (p_space_ids))
       AND (i.scope IS NULL OR i.scope = ANY (p_scopes)))
$$;
GRANT EXECUTE ON FUNCTION memsvc_context_stale(uuid[], text[], timestamptz) TO memsvc_app;
