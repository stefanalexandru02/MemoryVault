-- Spec 4.7: row-level security on every table with space_id.

CREATE OR REPLACE FUNCTION readable_spaces() RETURNS SETOF uuid LANGUAGE sql STABLE AS
  $$ SELECT space_id FROM memberships WHERE user_id = app_user_id() $$;

CREATE OR REPLACE FUNCTION writable_spaces() RETURNS SETOF uuid LANGUAGE sql STABLE AS
  $$ SELECT space_id FROM memberships WHERE user_id = app_user_id() AND role IN ('owner','writer') $$;

-- Apply the standard policy template to a space-scoped table. Modules adding a table with
-- space_id call this from their own migration:  SELECT memsvc_apply_space_rls('my_table');
-- write_check = 'writable' (default): writes need owner/writer membership.
-- write_check = 'readable': log-style tables a reader must be able to append to (retrieval_log, access_log).
CREATE OR REPLACE FUNCTION memsvc_apply_space_rls(tbl text, write_check text DEFAULT 'writable') RETURNS void
LANGUAGE plpgsql AS $$
DECLARE
  w text := CASE write_check WHEN 'readable' THEN 'readable_spaces' WHEN 'writable' THEN 'writable_spaces' END;
BEGIN
  IF w IS NULL THEN
    RAISE EXCEPTION 'write_check must be readable or writable, got %', write_check;
  END IF;
  EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', tbl);
  EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', tbl);
  EXECUTE format('CREATE POLICY sel ON %I FOR SELECT TO memsvc_app USING (space_id IN (SELECT readable_spaces()))', tbl);
  EXECUTE format('CREATE POLICY ins ON %I FOR INSERT TO memsvc_app WITH CHECK (space_id IN (SELECT %s()))', tbl, w);
  EXECUTE format('CREATE POLICY upd ON %I FOR UPDATE TO memsvc_app USING (space_id IN (SELECT %s()))', tbl, w);
  EXECUTE format('CREATE POLICY del ON %I FOR DELETE TO memsvc_app USING (space_id IN (SELECT %s()))', tbl, w);
END $$;

SELECT memsvc_apply_space_rls(t) FROM unnest(ARRAY[
  'sessions','events','observations','summaries','entities','facts','procedures','work_state',
  'profiles','embeddings','work_packets','memory_audit','procedure_hints']) AS t;

SELECT memsvc_apply_space_rls(t, 'readable') FROM unnest(ARRAY['retrieval_log','access_log']) AS t;

-- context_cache rows are per user (a render reflects that user's memberships).
ALTER TABLE context_cache ENABLE ROW LEVEL SECURITY;
ALTER TABLE context_cache FORCE ROW LEVEL SECURITY;
CREATE POLICY own ON context_cache FOR ALL TO memsvc_app
  USING (user_id = app_user_id() AND space_id IN (SELECT readable_spaces()))
  WITH CHECK (user_id = app_user_id() AND space_id IN (SELECT readable_spaces()));

-- Invalidation runs outside any user context (the API's LISTEN loop): SECURITY DEFINER so the app
-- role can delete every user's cached block for a changed scope. p_kind NULL = whole space.
CREATE OR REPLACE FUNCTION memsvc_invalidate_context(p_space uuid, p_kind text, p_key text) RETURNS int
LANGUAGE sql SECURITY DEFINER SET search_path = public, pg_temp AS $$
  WITH d AS (
    DELETE FROM context_cache
     WHERE p_space = ANY (space_ids)
       AND (p_kind IS NULL OR (p_kind || ':' || coalesce(p_key, '')) = ANY (scopes))
    RETURNING 1)
  SELECT count(*)::int FROM d
$$;

-- memberships: the app role sees only the caller's own rows (readable_spaces() and space resolution
-- need nothing more). Membership changes go through the system role (admin API/CLI).
ALTER TABLE memberships ENABLE ROW LEVEL SECURITY;
ALTER TABLE memberships FORCE ROW LEVEL SECURITY;
CREATE POLICY own ON memberships FOR SELECT TO memsvc_app USING (user_id = app_user_id());
