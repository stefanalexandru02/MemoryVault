-- Defense in depth for the role every user request runs as (memsvc_app). RLS already confines it to
-- the caller's spaces; this removes what it never needs, so a SQL injection somewhere in a user path
-- could not rewrite tenancy or tokens, and cannot call the cross-tenant SECURITY DEFINER functions
-- through another role.
--
-- Tenancy and token rows are written by the admin API/CLI, which run as memsvc_system. A user
-- request only reads them, plus the two bookkeeping updates below.
REVOKE INSERT, UPDATE, DELETE ON users, spaces, api_tokens, clients FROM memsvc_app;
GRANT UPDATE (last_used_at) ON api_tokens TO memsvc_app;               -- token verification
GRANT UPDATE (last_seen_at, current_project_key, current_project_at) ON clients TO memsvc_app;  -- sessions, events

-- Finished jobs are deleted by the retention run (memsvc_system).
REVOKE DELETE ON jobs FROM memsvc_app;

-- The cache invalidation functions see every user's context_cache rows. Only the runtime roles may
-- call them (functions are executable by PUBLIC by default).
REVOKE EXECUTE ON FUNCTION memsvc_invalidate_context(uuid, text, text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION memsvc_recall_invalidate(uuid, text, text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION memsvc_context_stale(uuid[], text[], timestamptz) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION memsvc_invalidate_context(uuid, text, text) TO memsvc_app, memsvc_system;
GRANT EXECUTE ON FUNCTION memsvc_recall_invalidate(uuid, text, text) TO memsvc_app, memsvc_system;
GRANT EXECUTE ON FUNCTION memsvc_context_stale(uuid[], text[], timestamptz) TO memsvc_app, memsvc_system;
