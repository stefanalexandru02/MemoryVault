-- Table and sequence privileges for the two runtime roles (spec 4.7). RLS still decides which rows
-- memsvc_app sees; memsvc_system bypasses it.
--
-- Existing objects are granted here; ALTER DEFAULT PRIVILEGES covers every table and sequence the
-- migrating role creates later (module migrations 0100+), so they need no GRANT of their own.
GRANT USAGE ON SCHEMA public TO memsvc_app, memsvc_system;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO memsvc_app, memsvc_system;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO memsvc_app, memsvc_system;

ALTER DEFAULT PRIVILEGES IN SCHEMA public
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO memsvc_app, memsvc_system;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
  GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO memsvc_app, memsvc_system;
