-- Repo rename: stevederico/shipyard → stevederico/detroit.
UPDATE github_repos SET repo = 'stevederico/detroit'
  WHERE repo = 'stevederico/shipyard';
UPDATE github_downloads SET repo = 'stevederico/detroit'
  WHERE repo = 'stevederico/shipyard';
UPDATE github_metrics SET repo = 'stevederico/detroit'
  WHERE repo = 'stevederico/shipyard';
