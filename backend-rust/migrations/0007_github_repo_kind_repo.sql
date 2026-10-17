-- dottie-desktop-releases, ahoy, clockwork are GitHub repos (not mac_app).
UPDATE github_repos SET kind = 'repo' WHERE repo IN (
    'stevederico/dottie-desktop-releases',
    'stevederico/ahoy',
    'stevederico/clockwork'
);
