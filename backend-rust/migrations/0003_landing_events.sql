-- Landing-page named events (Umami mirror). Companion to landing_visits:
-- landing_visits is one upsert row per visitor (pageview + UTM); landing_events
-- is the append-only stream of named custom events (download, scroll-depth,
-- outbound, time-on-page, ...) keyed to the same landing `iid`, so the desktop
-- attribution chain (iid -> install_id -> user) also covers event behaviour.
CREATE TABLE IF NOT EXISTS landing_events (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  iid        TEXT NOT NULL,
  name       TEXT NOT NULL,
  props      TEXT,
  path       TEXT,
  ip         TEXT,
  ua         TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_landing_events_name_created ON landing_events(name, created_at);
CREATE INDEX IF NOT EXISTS idx_landing_events_iid ON landing_events(iid);
