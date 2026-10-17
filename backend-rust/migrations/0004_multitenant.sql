-- Multi-tenant web analytics (Phase A of the Umami replacement).
--
-- dottie-analytics started single-tenant (one app: Dottie, with its rich
-- register/ping/install/landing funnel tables). This adds a GENERIC, per-site
-- events store so the one service can hold analytics for many apps (the ~45
-- sites currently in self-hosted Umami) without disturbing Dottie's schema.
--
-- `sites`      — one row per app/website. `id` reuses Umami's website_id (uuid
--                string) so imports are idempotent and cross-referenceable.
--                `write_key` is a per-site ingest secret for the future
--                `/collect` endpoint (Phase B live cutover); unused by import.
-- `site_events`— the generic event stream. `event_type` mirrors Umami (1 =
--                pageview, 2 = custom event). `distinct_id` is the visitor id
--                (Umami session.distinct_id — our landing d_iid where present —
--                else a synthetic `umami-<session_id>`). `session_id` groups a
--                visit. `props` is the event_data EAV rows collapsed to one JSON
--                object at import time. All timestamps are epoch-ms (created_at),
--                matching the rest of the schema, so the `/86400000` day-bucket
--                math in reports.rs works unchanged.
CREATE TABLE IF NOT EXISTS sites (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  domain     TEXT,
  write_key  TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_sites_write_key ON sites(write_key);

CREATE TABLE IF NOT EXISTS site_events (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  site_id      TEXT NOT NULL,
  event_type   INTEGER NOT NULL DEFAULT 2,
  name         TEXT,
  url          TEXT,
  referrer     TEXT,
  distinct_id  TEXT,
  session_id   TEXT,
  hostname     TEXT,
  browser      TEXT,
  os           TEXT,
  device       TEXT,
  country      TEXT,
  utm_source   TEXT,
  utm_medium   TEXT,
  utm_campaign TEXT,
  props        TEXT,
  created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_site_events_site_created ON site_events(site_id, created_at);
CREATE INDEX IF NOT EXISTS idx_site_events_site_name_created ON site_events(site_id, name, created_at);
CREATE INDEX IF NOT EXISTS idx_site_events_site_distinct ON site_events(site_id, distinct_id);
CREATE INDEX IF NOT EXISTS idx_site_events_site_url ON site_events(site_id, url);
