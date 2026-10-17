-- Base schema for dottie-analytics (sqlx migration).
--
-- DROP-IN SAFE: every statement is CREATE TABLE/INDEX IF NOT EXISTS, so against
-- an existing DB this is a no-op, and on a fresh DB it builds everything.
--
-- NOTE: the two PARTIAL unique indexes (idx_users_token / idx_events_event_id,
-- WHERE <col> IS NOT NULL) are deliberately NOT here. They reference columns
-- (token, event_id) that older DBs predate. On such an under-migrated DB the
-- CREATE TABLE IF NOT EXISTS above is a no-op, so the column is never added —
-- and a partial index on a missing column would fail mid-migration. Those
-- columns + indexes are handled idempotently after migrate, in
-- Db::ensure_schema (a faithful port of db.js's table_info-gated ALTERs).

CREATE TABLE IF NOT EXISTS users (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL,
  email      TEXT NOT NULL UNIQUE,
  install_id TEXT NOT NULL,
  version    TEXT,
  os_version TEXT,
  country    TEXT,
  token      TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_users_install ON users(install_id);

CREATE TABLE IF NOT EXISTS pings (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  install_id TEXT NOT NULL,
  version    TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pings_install ON pings(install_id, created_at);

CREATE TABLE IF NOT EXISTS events (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  install_id TEXT NOT NULL,
  name       TEXT NOT NULL,
  url        TEXT,
  props      TEXT,
  version    TEXT,
  created_at INTEGER NOT NULL,
  event_id   TEXT
);
CREATE INDEX IF NOT EXISTS idx_events_name_created ON events(name, created_at);
CREATE INDEX IF NOT EXISTS idx_events_install_created ON events(install_id, created_at);

CREATE TABLE IF NOT EXISTS diagnostics (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  install_id TEXT NOT NULL,
  version    TEXT,
  fingerprint TEXT,
  logs       TEXT,
  gateway    TEXT,
  note       TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_diagnostics_install_created ON diagnostics(install_id, created_at);

CREATE TABLE IF NOT EXISTS landing_visits (
  iid           TEXT PRIMARY KEY,
  ip            TEXT,
  ua            TEXT,
  path          TEXT,
  referrer      TEXT,
  utm_source    TEXT,
  utm_medium    TEXT,
  utm_campaign  TEXT,
  utm_term      TEXT,
  utm_content   TEXT,
  first_seen_at INTEGER NOT NULL,
  last_seen_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_landing_ip_seen ON landing_visits(ip, last_seen_at);

CREATE TABLE IF NOT EXISTS install_landing_map (
  install_id TEXT PRIMARY KEY,
  iid        TEXT NOT NULL,
  matched_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_install_map_iid ON install_landing_map(iid);

CREATE TABLE IF NOT EXISTS rejected_registrations (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT,
  email      TEXT,
  install_id TEXT,
  version    TEXT,
  os_version TEXT,
  country    TEXT,
  reason     TEXT NOT NULL,
  ip         TEXT,
  ua         TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rejected_created ON rejected_registrations(created_at);
CREATE INDEX IF NOT EXISTS idx_rejected_reason ON rejected_registrations(reason, created_at);
