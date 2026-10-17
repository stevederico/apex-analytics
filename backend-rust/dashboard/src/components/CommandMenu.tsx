// Cmd/Ctrl+K palette: jump to a tab or switch native / site / repo.
import { useEffect, useState } from 'react';
import {
  CommandDialog, Command, CommandInput, CommandList, CommandEmpty, CommandGroup, CommandItem,
} from '@stevederico/skateboard-ui/shadcn/ui/command';
import type { Site } from '../types';
import type { Scope } from '../lib/scope';
import { shortRepoName } from '../lib/scope';
import type { GrowthRepo } from './SiteSwitcher';
import { fmtNum } from '../lib/format';

interface Props {
  tabs: { id: string; label: string }[];
  sites: Site[];
  repos: GrowthRepo[];
  onSelectTab: (id: string) => void;
  onSelectScope: (scope: Scope) => void;
}

export function CommandMenu({ tabs, sites, repos, onSelectTab, onSelectScope }: Props) {
  const [open, setOpen] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setOpen((o) => !o);
      }
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, []);

  const pickTab = (id: string) => { onSelectTab(id); setOpen(false); };
  const pickScope = (s: Scope) => { onSelectScope(s); setOpen(false); };

  return (
    <CommandDialog open={open} onOpenChange={setOpen} title="Command menu" description="Jump to a tab or switch app">
      <Command>
        <CommandInput placeholder="Jump to tab or switch site/repo…" />
        <CommandList>
          <CommandEmpty>No results.</CommandEmpty>
          <CommandGroup heading="Tabs">
            {tabs.map((t) => (
              <CommandItem key={t.id} value={`tab ${t.label}`} onSelect={() => pickTab(t.id)}>{t.label}</CommandItem>
            ))}
          </CommandGroup>
          <CommandGroup heading="Mac">
            <CommandItem value="scope Dottie Desktop mac" onSelect={() => pickScope({ kind: 'native', id: '' })}>
              Dottie Desktop
            </CommandItem>
          </CommandGroup>
          {sites.length > 0 && (
            <CommandGroup heading="Sites">
              {sites.map((s) => (
                <CommandItem
                  key={s.id}
                  value={`site ${s.name}`}
                  onSelect={() => pickScope({ kind: 'site', id: s.id })}
                >
                  {s.name} · {fmtNum(s.events)}
                </CommandItem>
              ))}
            </CommandGroup>
          )}
          {repos.length > 0 && (
            <CommandGroup heading="Repos">
              <CommandItem
                value="repo all repos"
                onSelect={() => pickScope({ kind: 'repo', id: '' })}
              >
                All repos
              </CommandItem>
              {repos.map((r) => (
                <CommandItem
                  key={r.repo}
                  value={`repo ${r.repo} ${shortRepoName(r.repo)}`}
                  onSelect={() => pickScope({ kind: 'repo', id: r.repo })}
                >
                  {shortRepoName(r.repo)}
                </CommandItem>
              ))}
            </CommandGroup>
          )}
        </CommandList>
      </Command>
    </CommandDialog>
  );
}
