-- context_invalidations carries space_id, so it gets forced RLS like every space-scoped table (4.7).
-- memsvc_app has no grants on it and reaches it only through the SECURITY DEFINER functions of 0201,
-- whose owner bypasses RLS; the policy keeps the invariant if a grant is ever added.
ALTER TABLE context_invalidations ENABLE ROW LEVEL SECURITY;
ALTER TABLE context_invalidations FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS sel ON context_invalidations;
CREATE POLICY sel ON context_invalidations FOR SELECT TO memsvc_app
  USING (space_id IS NULL OR space_id IN (SELECT readable_spaces()));
