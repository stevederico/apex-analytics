-- Dottie Pro relay quota: per-user, per-UTC-day request + token counters.
-- One row per (user_id, day); `day` is 'YYYY-MM-DD' produced by SQLite's
-- strftime('%Y-%m-%d','now'), which is UTC, so the window rolls at 00:00 UTC
-- without a date crate. Counters only ever move forward; rows are never read
-- across days, so no cleanup job is needed (a stale row is a few bytes).

CREATE TABLE IF NOT EXISTS pro_usage (
    user_id  INTEGER NOT NULL,
    day      TEXT NOT NULL,
    requests INTEGER NOT NULL DEFAULT 0,
    tokens   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, day)
);
