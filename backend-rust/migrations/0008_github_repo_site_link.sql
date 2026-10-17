-- Optional link: GitHub growth repo → web-analytics site (or native desktop).
-- linked_site_id = sites.id, or the sentinel "native" for Dottie desktop.
ALTER TABLE github_repos ADD COLUMN linked_site_id TEXT;

-- Known product pairs (Umami site ids are stable; native = desktop app scope).
UPDATE github_repos SET linked_site_id = '535354f8-732e-4630-b37a-b837ca1db1ba'
  WHERE repo = 'stevederico/xqr';
UPDATE github_repos SET linked_site_id = 'b7456692-80f2-4bd1-a714-a90dcbe18a45'
  WHERE repo = 'stevederico/spyglass';
UPDATE github_repos SET linked_site_id = '6ba4f501-688a-4b7c-a87a-401c11ee4e83'
  WHERE repo = 'stevederico/scorecard';
UPDATE github_repos SET linked_site_id = 'native'
  WHERE repo = 'stevederico/dottie-desktop-releases';
