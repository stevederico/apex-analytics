-- Umami import identity. NULL on live /api/collect and /api/send rows.
-- Partial unique index: the same Umami event_id cannot land twice on one site.
-- Rows imported before this column existed stay NULL and are matched by the
-- import path's natural key (site, created_at, type, session, url, name).
ALTER TABLE site_events ADD COLUMN source_event_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_site_events_source_id
  ON site_events(site_id, source_event_id)
  WHERE source_event_id IS NOT NULL;
