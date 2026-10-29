/** Header switcher scope: Dottie native, a web site, or a GitHub growth repo. */
import { createContext, useContext, type ReactNode } from 'react';

export type ScopeKind = 'native' | 'site' | 'repo';

export interface Scope {
  kind: ScopeKind;
  /** site id, or owner/name for repos; empty id + kind repo = all repos */
  id: string;
}

const KEY = 'apex_dash_scope';
/** Legacy key used before repos joined the switcher. */
const LEGACY_SITE_KEY = 'apex_dash_site';

export function loadScope(): Scope {
  const raw = localStorage.getItem(KEY);
  if (raw != null) return parseScope(raw);
  const legacy = localStorage.getItem(LEGACY_SITE_KEY);
  if (legacy) {
    const s: Scope = { kind: 'site', id: legacy };
    saveScope(s);
    return s;
  }
  return { kind: 'native', id: '' };
}

export function saveScope(s: Scope): void {
  localStorage.setItem(KEY, encodeScope(s));
  if (s.kind === 'site') localStorage.setItem(LEGACY_SITE_KEY, s.id);
  else localStorage.setItem(LEGACY_SITE_KEY, '');
}

export function encodeScope(s: Scope): string {
  if (s.kind === 'native') return '';
  if (s.kind === 'site') return `site:${s.id}`;
  return s.id ? `repo:${s.id}` : 'repo:';
}

export function parseScope(raw: string): Scope {
  if (!raw) return { kind: 'native', id: '' };
  if (raw === 'repo:' || raw === 'repo') return { kind: 'repo', id: '' };
  if (raw.startsWith('repo:')) return { kind: 'repo', id: raw.slice(5) };
  if (raw.startsWith('site:')) return { kind: 'site', id: raw.slice(5) };
  return { kind: 'site', id: raw };
}

export function isAllRepos(s: Scope): boolean {
  return s.kind === 'repo' && !s.id;
}

export function shortRepoName(full: string): string {
  if (!full) return 'All repos';
  const i = full.indexOf('/');
  return i >= 0 ? full.slice(i + 1) : full;
}

/** Lets Overview (growth) deep-link into a specific repo from the all-repos table. */
export const ScopeNavContext = createContext<(s: Scope) => void>(() => {});

export function ScopeNavProvider({
  onSelect,
  children,
}: {
  onSelect: (s: Scope) => void;
  children: ReactNode;
}) {
  return (
    <ScopeNavContext.Provider value={onSelect}>{children}</ScopeNavContext.Provider>
  );
}

export function useScopeNav(): (s: Scope) => void {
  return useContext(ScopeNavContext);
}
