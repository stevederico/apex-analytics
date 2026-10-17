-- Auto-disable for tracked repos GitHub answers 404 for (gone private or
-- renamed): a deterministic failure that otherwise errors every collector
-- cycle forever. Disabled repos keep their download/metric history and are
-- skipped by the collectors; any PATCH to the row re-enables it.
ALTER TABLE github_repos ADD COLUMN disabled_at TEXT;
