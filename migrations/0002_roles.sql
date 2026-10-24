-- Spec 4.7. Roles are cluster-wide: create only when missing, and tolerate a concurrent creator
-- (parallel test databases on one cluster run this at the same time).
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'memsvc_app') THEN
    BEGIN
      CREATE ROLE memsvc_app NOINHERIT LOGIN;                 -- API and MCP
    EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
    END;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'memsvc_system') THEN
    BEGIN
      CREATE ROLE memsvc_system NOINHERIT LOGIN BYPASSRLS;    -- broker, planner, migrations
    EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
    END;
  END IF;
END $$;
