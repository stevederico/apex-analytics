-- Growth-chart merge: GitHub release downloads + stars/forks/clones/views.
-- Ported from growth-chart/backend/github.ts schema.

CREATE TABLE IF NOT EXISTS github_repos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS github_downloads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL DEFAULT '',
    date TEXT NOT NULL,
    tag TEXT NOT NULL,
    download_count INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_github_downloads_repo_date_tag
    ON github_downloads(repo, date, tag);

CREATE TABLE IF NOT EXISTS github_metrics (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    date TEXT NOT NULL,
    metric TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    uniques INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_github_metrics_repo_date_metric
    ON github_metrics(repo, date, metric);
