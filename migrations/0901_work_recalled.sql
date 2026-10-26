-- Work recalled (modules ingest and recall): how much session work the memories whose full body reached the model
-- stand for, next to the tokens served (GET /v1/stats/recall `work`).
--
-- work_tokens: estimated tokens (memvault_core::estimate_tokens, chars / 4) of the session events a memory was made
-- from. NULL = unknown: a memory written by hand, a day or week summary, events that are gone. Documents have no
-- column: an ingested source is not session work.
ALTER TABLE observations ADD COLUMN IF NOT EXISTS work_tokens int;
ALTER TABLE summaries ADD COLUMN IF NOT EXISTS work_tokens int;
ALTER TABLE facts ADD COLUMN IF NOT EXISTS work_tokens int;
ALTER TABLE procedures ADD COLUMN IF NOT EXISTS work_tokens int;

-- Backfill from the events that still exist. The extract applier estimates the events as the window showed them to
-- the extractor; here the estimate ingest stored for each event's payload stands in (retention trims a payload but
-- keeps it). An observation citing an event that is gone stays NULL.
UPDATE observations o
   SET work_tokens = (SELECT sum(e.token_estimate) FROM events e WHERE e.id = ANY (o.source_event_ids))
 WHERE o.work_tokens IS NULL AND cardinality(o.source_event_ids) > 0
   AND (SELECT count(*) FROM events e WHERE e.id = ANY (o.source_event_ids))
     = (SELECT count(DISTINCT x) FROM unnest(o.source_event_ids) AS x);

-- A prompt summary: the events of the windows of its prompt; a session summary: every event of the session.
UPDATE summaries s
   SET work_tokens = (
     SELECT sum(e.token_estimate) FROM events e
      WHERE e.session_id = s.session_id
        AND (s.grain = 'session'
             OR e.window_no IN (SELECT w.window_no FROM session_windows w
                                 WHERE w.session_id = s.session_id AND w.prompt_no = s.prompt_no)))
 WHERE s.work_tokens IS NULL AND s.session_id IS NOT NULL AND s.grain IN ('prompt', 'session');

-- Facts and procedures: the sum over the distinct observations they were derived from (their evidence), kept up to
-- date by a trigger whichever writer (extract, consolidation, remember, hard delete) changes that list.
UPDATE facts f
   SET work_tokens = (SELECT sum(o.work_tokens) FROM observations o WHERE o.id = ANY (f.evidence_observation_ids))
 WHERE cardinality(f.evidence_observation_ids) > 0;
UPDATE procedures p
   SET work_tokens = (SELECT sum(o.work_tokens) FROM observations o WHERE o.id = ANY (p.source_observation_ids))
 WHERE cardinality(p.source_observation_ids) > 0;
CREATE OR REPLACE FUNCTION memsvc_derived_work_tokens() RETURNS trigger
LANGUAGE plpgsql SET search_path = public, pg_temp AS $$
BEGIN
  IF TG_TABLE_NAME = 'facts' THEN
    NEW.work_tokens := (SELECT sum(o.work_tokens) FROM observations o WHERE o.id = ANY (NEW.evidence_observation_ids));
  ELSE
    NEW.work_tokens := (SELECT sum(o.work_tokens) FROM observations o WHERE o.id = ANY (NEW.source_observation_ids));
  END IF;
  RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS facts_work_tokens ON facts;
CREATE TRIGGER facts_work_tokens BEFORE INSERT OR UPDATE OF evidence_observation_ids ON facts
  FOR EACH ROW EXECUTE FUNCTION memsvc_derived_work_tokens();
DROP TRIGGER IF EXISTS procedures_work_tokens ON procedures;
CREATE TRIGGER procedures_work_tokens BEFORE INSERT OR UPDATE OF source_observation_ids ON procedures
  FOR EACH ROW EXECUTE FUNCTION memsvc_derived_work_tokens();

-- Recalled memories. access_log already records every content read; recalled_ids is the part of a row whose full
-- body reached the model (memory_get bodies, expanded recall bodies, the profile facts and procedures of a
-- session-start block), session_id the session the call was counted in (NULL: none open, counted per user and
-- UTC day). The stats count each (session, memory) once, so a repeated fetch does not add up.
ALTER TABLE access_log ADD COLUMN IF NOT EXISTS session_id uuid;
ALTER TABLE access_log ADD COLUMN IF NOT EXISTS recalled_ids uuid[] NOT NULL DEFAULT '{}';
CREATE INDEX IF NOT EXISTS access_log_recalled ON access_log (client_id, created_at) WHERE recalled_ids <> '{}';

-- A cached session-start block remembers the memories it renders in full, so a cache hit counts them too.
ALTER TABLE context_cache ADD COLUMN IF NOT EXISTS recalled_ids uuid[] NOT NULL DEFAULT '{}';
