#!/bin/sh
# First-boot setup for the local compose stack. The postgres entrypoint runs this once, when the data
# volume is empty. It plays the part CloudNativePG plays in Kubernetes: the two runtime roles, the
# memsvc database owned by memsvc_system, and the extensions (pg_search only when the image has it).
set -eu

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres \
  -v app_pw="$MEMSVC_APP_PASSWORD" -v sys_pw="$MEMSVC_SYSTEM_PASSWORD" <<'SQL'
CREATE ROLE memsvc_app LOGIN NOINHERIT PASSWORD :'app_pw';
CREATE ROLE memsvc_system LOGIN NOINHERIT BYPASSRLS PASSWORD :'sys_pw';
CREATE DATABASE memsvc OWNER memsvc_system;
SQL

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname memsvc <<'SQL'
CREATE EXTENSION IF NOT EXISTS vector;
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pg_search') THEN
    CREATE EXTENSION IF NOT EXISTS pg_search;
  ELSE
    RAISE NOTICE 'pg_search not installed; BM25 falls back to tsvector';
  END IF;
END $$;
SQL
