-- Helpers used by generated columns and RLS.

-- array_to_string is STABLE in core; generated columns need IMMUTABLE. For text[] it is immutable.
CREATE OR REPLACE FUNCTION memsvc_join(arr text[], sep text) RETURNS text
  LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT array_to_string(arr, sep) $$;

CREATE OR REPLACE FUNCTION app_user_id() RETURNS uuid LANGUAGE sql STABLE AS
  $$ SELECT nullif(current_setting('app.user_id', true), '')::uuid $$;

CREATE OR REPLACE FUNCTION app_client_id() RETURNS uuid LANGUAGE sql STABLE AS
  $$ SELECT nullif(current_setting('app.client_id', true), '')::uuid $$;
