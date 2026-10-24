-- Recall module (spec 11.6): what the worker needs to re-render an invalidated SessionStart block.
--
-- context_cache rows are deleted when a write NOTIFYs memory_invalidate. To re-render "known keys"
-- after the 2 s debounce, the API listener turns every deleted row into a queued
-- jobs(type='render_context') whose payload names the block (user, client, project, device, source);
-- the worker calls memvault_recall::cache::rerender with it.
ALTER TABLE context_cache ADD COLUMN IF NOT EXISTS client_id uuid REFERENCES clients(id) ON DELETE CASCADE;
ALTER TABLE context_cache ADD COLUMN IF NOT EXISTS project_key text;
CREATE INDEX IF NOT EXISTS context_cache_rendered_at ON context_cache (rendered_at);
CREATE INDEX IF NOT EXISTS jobs_render_context_queued ON jobs ((payload->>'cache_key'))
  WHERE type = 'render_context' AND status = 'queued';

-- Delete the cached blocks matching a NOTIFY payload and return what they were, so the caller can
-- queue a re-render. SECURITY DEFINER: the listener has no user context and must reach every user's
-- rows. p_space NULL = every row (used after the listener reconnects, when notifications may have
-- been lost); p_kind NULL = the whole space.
CREATE OR REPLACE FUNCTION memsvc_recall_invalidate(p_space uuid, p_kind text, p_key text)
RETURNS TABLE (cache_key text, user_id uuid, client_id uuid, space_id uuid, project_key text, device_id text,
               source text, rendered_at timestamptz)
LANGUAGE sql SECURITY DEFINER SET search_path = public, pg_temp AS $$
  DELETE FROM context_cache c
   WHERE (p_space IS NULL OR p_space = ANY (c.space_ids))
     AND (p_kind IS NULL OR (p_kind || ':' || coalesce(p_key, '')) = ANY (c.scopes))
  RETURNING c.cache_key, c.user_id, c.client_id, c.space_id, c.project_key, c.device_id, c.source, c.rendered_at
$$;
