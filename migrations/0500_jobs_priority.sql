-- Spec 13: a reindex backfill runs at low priority. `claim` takes lower numbers first, so the embed
-- jobs of fresh rows (0) are never queued behind a large backfill (10).
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS priority smallint NOT NULL DEFAULT 0;
