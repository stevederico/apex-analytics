-- Classify growth GitHub repos: website | repo | mac_app.
ALTER TABLE github_repos ADD COLUMN kind TEXT NOT NULL DEFAULT 'repo';

-- Known seeds (same list as growth-chart) — mac apps vs libraries.
UPDATE github_repos SET kind = 'mac_app' WHERE repo IN (
    'stevederico/dottie-desktop-releases',
    'stevederico/ahoy',
    'stevederico/clockwork'
);
UPDATE github_repos SET kind = 'repo' WHERE kind = 'repo' OR kind = '';
