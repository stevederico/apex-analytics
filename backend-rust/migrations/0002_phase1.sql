-- Phase 1 schema: error-issue lifecycle tracking.
--
-- DROP-IN SAFE: CREATE ... IF NOT EXISTS, so applying against a DB that already
-- ran 0001 (incl. the live prod DB) only adds the new `issues` table and never
-- touches existing data. `install_landing_map` already exists (from 0001) and is
-- reused for attribution — no new table needed there.
--
-- ISSUES: one row per distinct error signature (fingerprint = name|file|function
-- |line). record_event upserts here for every `error.%` event: first sighting
-- inserts status='active', later sightings bump count/last_seen. An operator can
-- PATCH status to 'resolved'; if the same fingerprint recurs afterwards the
-- upsert flips it to 'regressed' (a resolved bug came back). This is pure
-- app/SQL logic — no ClickHouse needed (see docs/gap-analysis-2026-07.md).

CREATE TABLE IF NOT EXISTS issues (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  fingerprint TEXT NOT NULL UNIQUE,
  name        TEXT NOT NULL,
  file        TEXT,
  function    TEXT,
  line        TEXT,
  status      TEXT NOT NULL DEFAULT 'active',  -- active | resolved | regressed
  count       INTEGER NOT NULL DEFAULT 0,
  first_seen  INTEGER NOT NULL,
  last_seen   INTEGER NOT NULL,
  resolved_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_issues_status_last ON issues(status, last_seen);
