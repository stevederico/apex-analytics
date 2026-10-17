// Unified header switcher: Dottie Desktop · web sites · GitHub repos.
import type { Site } from '../types';
import type { Scope } from '../lib/scope';
import { shortRepoName } from '../lib/scope';
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuLabel,
} from '@stevederico/skateboard-ui/shadcn/ui/dropdown-menu';

export interface GrowthRepo {
  id: number;
  repo: string;
  kind: string;
  /** sites.id, `"native"` for Dottie desktop, or null */
  linked_site_id?: string | null;
}

interface Props {
  scope: Scope;
  sites: Site[];
  repos: GrowthRepo[];
  onSelect: (scope: Scope) => void;
}

const KIND_PILL: Record<string, string> = {
  website: 'site',
  repo: 'repo',
  mac_app: 'mac',
};

const Pill = ({ label }: { label: string }) => (
  <span className="border border-app text-app text-[9px] font-mono uppercase tracking-[0.5px] leading-none px-1 py-0.5">
    {label}
  </span>
);

/** Human label for the current header scope (also used for document.title). */
export function scopeLabel(scope: Scope, sites: Site[]): string {
  if (scope.kind === 'native') return 'Dottie Desktop';
  if (scope.kind === 'site') {
    // Never fall back to the raw site id (uuid): before /api/sites resolves the
    // list is empty, so the uuid would flash in the navbar until the name loads.
    // Show a neutral placeholder instead; the name appears once the list is in
    // (and a genuinely-missing site is reset to native by Shell).
    return sites.find((s) => s.id === scope.id)?.name ?? 'Loading…';
  }
  if (!scope.id) return 'All repos';
  return shortRepoName(scope.id);
}

export function SiteSwitcher({ scope, sites, repos, onSelect }: Props) {
  const label = scopeLabel(scope, sites);
  const sortedSites = [...sites].sort((a, b) => a.name.localeCompare(b.name));
  const sortedRepos = [...repos].sort((a, b) => a.repo.localeCompare(b.repo));
  const allReposActive = scope.kind === 'repo' && !scope.id;

  const pill =
    scope.kind === 'native'
      ? 'desktop'
      : scope.kind === 'site'
        ? 'site'
        : allReposActive
          ? 'repos'
          : KIND_PILL[repos.find((r) => r.repo === scope.id)?.kind ?? 'repo'] ?? 'repo';

  return (
    <DropdownMenu>
      <DropdownMenuTrigger className="flex items-center gap-2 bg-transparent border-0 text-foreground font-mono text-sm font-bold tracking-[0.5px] uppercase px-0.5 cursor-pointer hover:text-app outline-none max-w-[min(50vw,20rem)]">
        <span className="truncate">{label}</span>
        <Pill label={pill} />
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" className="opacity-60 shrink-0"><path d="m6 9 6 6 6-6" /></svg>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="min-w-56 max-h-[70vh] overflow-y-auto">
        <DropdownMenuLabel className="font-mono text-[10px] uppercase tracking-[0.5px] text-muted-foreground">
          Mac
        </DropdownMenuLabel>
        <DropdownMenuItem
          onSelect={() => onSelect({ kind: 'native', id: '' })}
          data-active={scope.kind === 'native'}
          className="font-mono text-xs data-[active=true]:text-app"
        >
          Dottie Desktop
          <span className="text-muted-foreground ml-auto pl-2">mac</span>
        </DropdownMenuItem>

        {sortedSites.length > 0 && (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuLabel className="font-mono text-[10px] uppercase tracking-[0.5px] text-muted-foreground">
              Sites
            </DropdownMenuLabel>
            {sortedSites.map((s) => (
              <DropdownMenuItem
                key={`site:${s.id}`}
                onSelect={() => onSelect({ kind: 'site', id: s.id })}
                data-active={scope.kind === 'site' && scope.id === s.id}
                className="font-mono text-xs data-[active=true]:text-app"
              >
                {s.name}
                <span className="text-muted-foreground ml-auto pl-2">{KIND_PILL.website}</span>
              </DropdownMenuItem>
            ))}
          </>
        )}

        {sortedRepos.length > 0 && (
          <>
            <DropdownMenuSeparator />
            {/* Header row: label left, “All repos” action on the right */}
            <DropdownMenuItem
              onSelect={() => onSelect({ kind: 'repo', id: '' })}
              data-active={allReposActive}
              className="font-mono text-[10px] uppercase tracking-[0.5px] data-[active=true]:text-app flex items-center justify-between gap-3 cursor-pointer"
            >
              <span className="text-muted-foreground">Repos</span>
              <span className="text-app normal-case tracking-normal text-xs font-bold">
                All repos →
              </span>
            </DropdownMenuItem>
            {sortedRepos.map((r) => (
              <DropdownMenuItem
                key={`repo:${r.repo}`}
                onSelect={() => onSelect({ kind: 'repo', id: r.repo })}
                data-active={scope.kind === 'repo' && scope.id === r.repo}
                className="font-mono text-xs data-[active=true]:text-app"
              >
                {shortRepoName(r.repo)}
                <span className="text-muted-foreground ml-auto pl-2">
                  {KIND_PILL[r.kind] ?? 'repo'}
                </span>
              </DropdownMenuItem>
            ))}
          </>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
