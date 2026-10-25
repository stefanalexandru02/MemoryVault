-- Consolidation (spec 12): audit ordering and revert bookkeeping, decay bookkeeping, hint
-- consumption bookkeeping.

-- Audit rows of one run are replayed in reverse by `seq` (now() is the same for a whole
-- transaction and UUIDv7 ids are only ordered per process).
ALTER TABLE memory_audit ADD COLUMN IF NOT EXISTS seq bigserial;
CREATE INDEX IF NOT EXISTS memory_audit_run_seq ON memory_audit (run_id, seq) WHERE run_id IS NOT NULL;

-- A `revert` row points at the audit row it undid, so reverting a run twice undoes nothing twice.
ALTER TABLE memory_audit ADD COLUMN IF NOT EXISTS reverts_audit_id uuid;
CREATE INDEX IF NOT EXISTS memory_audit_reverts ON memory_audit (reverts_audit_id) WHERE reverts_audit_id IS NOT NULL;

-- Decay (12.1 step 2) is applied incrementally: `decayed_at` is the last time importance was
-- decayed, so a nightly run only decays the time elapsed since the previous one.
ALTER TABLE observations ADD COLUMN IF NOT EXISTS decayed_at timestamptz;
ALTER TABLE facts ADD COLUMN IF NOT EXISTS decayed_at timestamptz;

-- Procedure hints consumed by a procedures packet remember the run, so a revert releases them.
ALTER TABLE procedure_hints ADD COLUMN IF NOT EXISTS consumed_run_id uuid;

-- Packets of a consolidation run carry `run_id` in their (immutable) inputs.
CREATE INDEX IF NOT EXISTS work_packets_run_id ON work_packets ((inputs->>'run_id')) WHERE inputs ? 'run_id';
