// App shell: sticky header with unified switcher (native · sites · repos),
// sidebar tabs for the active scope, Settings, Cmd+K.
import { useEffect, useRef, useState } from 'react';
import { api, clearKey } from '../api';
import type { SitesResponse } from '../types';
import { useAsync } from '../lib/async';
import { DOTTIE_TABS, SITE_TABS, REPO_TABS, type Tab } from '../views/registry';
import Growth from '../views/Growth';
import { HeaderSlotContext } from '../lib/headerSlot';
import { loadScope, saveScope, ScopeNavProvider, type Scope } from '../lib/scope';
import { CommandMenu } from './CommandMenu';
import { SiteSwitcher, scopeLabel, type GrowthRepo } from './SiteSwitcher';
import { AddSiteButton } from './AddSiteButton';
import Settings from '../views/Settings';

interface Props {
  current: string;
  navigate: (id: string) => void;
  onSignOut: () => void;
}

/** Repo linked to this scope (`native` or a site uuid). */
function linkedRepoFor(scope: Scope, repos: GrowthRepo[]): string | null {
  if (scope.kind === 'native') {
    return repos.find((r) => r.linked_site_id === 'native')?.repo ?? null;
  }
  if (scope.kind === 'site') {
    return repos.find((r) => r.linked_site_id === scope.id)?.repo ?? null;
  }
  return null;
}

function tabsFor(scope: Scope, linkedRepo: string | null): Tab[] {
  const repoTab: Tab | null = linkedRepo
    ? { id: 'linked_repo', label: 'Repo', View: Growth }
    : null;
  if (scope.kind === 'repo') return REPO_TABS;
  if (scope.kind === 'site') {
    if (!repoTab) return SITE_TABS;
    return [SITE_TABS[0]!, repoTab, ...SITE_TABS.slice(1)];
  }
  // native
  if (!repoTab) return DOTTIE_TABS;
  return [DOTTIE_TABS[0]!, repoTab, ...DOTTIE_TABS.slice(1)];
}

function viewProp(scope: Scope, currentTab: string, linkedRepo: string | null): string {
  if (currentTab === 'linked_repo' && linkedRepo) return linkedRepo;
  if (scope.kind === 'site') return scope.id;
  if (scope.kind === 'repo') return scope.id;
  return '';
}

export function Shell({ current, navigate, onSignOut }: Props) {
  const [scope, setScope] = useState<Scope>(() => loadScope());
  const [slotEl, setSlotEl] = useState<HTMLElement | null>(null);
  const [sitesTick, setSitesTick] = useState(0);
  /** Site id just created via “+” — skip stale-list drop until list reloads. */
  const pendingNewSite = useRef<string | null>(null);

  // Settings / header “+” dispatch this so the switcher picks up new properties.
  useEffect(() => {
    const onSites = () => setSitesTick((t) => t + 1);
    window.addEventListener('dottie:sites-changed', onSites);
    return () => window.removeEventListener('dottie:sites-changed', onSites);
  }, []);

  const sitesState = useAsync(
    () => api<SitesResponse>('/api/sites').then((r) => r.sites).catch(() => []),
    [sitesTick],
  );
  const sites = sitesState.data ?? [];

  const reposState = useAsync(async (): Promise<GrowthRepo[]> => {
    try {
      const r = await api<{ repos: GrowthRepo[] }>('/api/repos');
      const raw = r.repos;
      if (!Array.isArray(raw)) return [];
      return raw.map((x) => ({
        id: Number(x.id) || 0,
        repo: String(x.repo),
        kind: String(x.kind || 'repo'),
        linked_site_id: x.linked_site_id != null && x.linked_site_id !== ''
          ? String(x.linked_site_id)
          : null,
      }));
    } catch {
      return [];
    }
  }, []);
  const repos = reposState.data ?? [];
  const linkedRepo = linkedRepoFor(scope, repos);

  // Drop remembered site/repo that no longer exists.
  useEffect(() => {
    if (scope.kind === 'site' && sitesState.data) {
      if (pendingNewSite.current === scope.id) {
        if (sitesState.data.some((s) => s.id === scope.id)) {
          pendingNewSite.current = null;
        }
        return; // list still catching up after create
      }
      if (!sitesState.data.some((s) => s.id === scope.id)) {
        const next: Scope = { kind: 'native', id: '' };
        setScope(next);
        saveScope(next);
      }
    }
  }, [sitesState.data, scope]);

  useEffect(() => {
    // All-repos scope (empty id) is always valid.
    if (scope.kind === 'repo' && scope.id && reposState.data && reposState.data.length > 0) {
      if (!reposState.data.some((r) => r.repo === scope.id)) {
        const next: Scope = { kind: 'native', id: '' };
        setScope(next);
        saveScope(next);
      }
    }
  }, [reposState.data, scope]);

  const tabs = tabsFor(scope, linkedRepo);
  const isSettings = current === 'settings';
  const active = tabs.find((t) => t.id === current) ?? tabs[0]!;
  const View = active.View;

  // Browser tab: "Overview · Dottie Desktop · Dottie Analytics"
  useEffect(() => {
    const scopeName = scopeLabel(scope, sites);
    const tabName = isSettings ? 'Settings' : active.label;
    document.title = `${tabName} · ${scopeName} · Dottie Analytics`;
    return () => {
      document.title = 'Dottie Analytics';
    };
  }, [scope, sites, isSettings, active.label]);

  const switchScope = (next: Scope) => {
    setScope(next);
    saveScope(next);
    navigate(tabsFor(next, linkedRepoFor(next, repos))[0]!.id);
  };

  const signOut = () => { clearKey(); onSignOut(); };
  const navBtn = 'text-left font-mono uppercase tracking-[0.5px] text-xs px-3 py-1.5';

  return (
    <ScopeNavProvider onSelect={switchScope}>
      <HeaderSlotContext.Provider value={slotEl}>
        <CommandMenu
          tabs={[...tabs, { id: 'settings', label: 'Settings' }]}
          sites={sites}
          repos={repos}
          onSelectTab={navigate}
          onSelectScope={switchScope}
        />

        <div className="flex flex-col h-dvh">
          <header className="flex items-center gap-2 px-4.5 py-3 border-b border-border bg-background shrink-0">
            <SiteSwitcher
              scope={scope}
              sites={sites}
              repos={repos}
              onSelect={switchScope}
            />
            <AddSiteButton
              onCreated={(site) => {
                pendingNewSite.current = site.id;
                switchScope({ kind: 'site', id: site.id });
              }}
            />
            <span className="flex-1" />
            <div ref={setSlotEl} className="flex items-center gap-2.5 flex-wrap justify-end empty:hidden" />
          </header>

          <div className="flex flex-1 min-h-0 max-[640px]:flex-col">
            <nav className="flex flex-col w-[188px] shrink-0 p-3 border-r border-border overflow-y-auto max-[640px]:w-full max-[640px]:flex-row max-[640px]:flex-wrap max-[640px]:border-r-0 max-[640px]:border-b max-[640px]:overflow-visible">
              <div className="flex flex-col gap-1 max-[640px]:flex-row max-[640px]:flex-wrap">
                {tabs.map((t) => (
                  <button
                    key={t.id}
                    data-sidebar="menu-button"
                    data-active={!isSettings && t.id === active.id}
                    onClick={() => navigate(t.id)}
                    className={navBtn}
                  >{t.label}</button>
                ))}
              </div>
              <div className="mt-auto pt-3 max-[640px]:mt-0 max-[640px]:pt-0 max-[640px]:w-full">
                <button
                  data-sidebar="menu-button"
                  data-active={isSettings}
                  onClick={() => navigate('settings')}
                  className={`${navBtn} flex items-center gap-2 w-full`}
                >
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg>
                  Settings
                </button>
              </div>
            </nav>

            <main className="flex-1 min-w-0 overflow-y-auto [scrollbar-gutter:stable] p-4.5">
              <div className="max-w-[1100px]">
                {isSettings
                  ? <Settings onSignOut={signOut} />
                  : (
                    <View
                      key={`${scope.kind}:${scope.id}:${active.id}`}
                      site={viewProp(scope, active.id, linkedRepo)}
                    />
                  )}
              </div>
            </main>
          </div>
        </div>
      </HeaderSlotContext.Provider>
    </ScopeNavProvider>
  );
}
