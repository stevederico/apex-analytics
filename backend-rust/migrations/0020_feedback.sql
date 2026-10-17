-- In-app feedback (absorbed from the standalone feedback-assistant service).
-- One row per widget submission. `site_id` is the site whose write_key the
-- widget posted with (the same key `/script.js` uses), so feedback lands next
-- to that site's analytics. `status` is the triage state: new | read | archived.
-- The optional screenshot is stored inline as a BLOB (≤2 MiB, JPEG/PNG) so the
-- DB stays the only state (distroless image, no uploads dir) and the
-- `/api/ops/db/snapshot` copy carries it. It is the LAST column, so list
-- queries that never select it don't read its overflow pages.
CREATE TABLE IF NOT EXISTS feedback (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  site_id          TEXT NOT NULL,
  message          TEXT NOT NULL,
  url              TEXT,
  user_agent       TEXT,
  app_version      TEXT,
  end_user_id      TEXT,
  end_user_name    TEXT,
  end_user_email   TEXT,
  country          TEXT,
  status           TEXT NOT NULL DEFAULT 'new',
  created_at       INTEGER NOT NULL,
  screenshot_type  TEXT,
  screenshot_bytes INTEGER,
  screenshot       BLOB
);
CREATE INDEX IF NOT EXISTS idx_feedback_site_created ON feedback(site_id, created_at);
CREATE INDEX IF NOT EXISTS idx_feedback_created ON feedback(created_at);
