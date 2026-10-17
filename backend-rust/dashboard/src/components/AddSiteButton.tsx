// “+” next to the app switcher — modal to create a web-analytics property.
import { useState } from 'react';
import { Button } from '@stevederico/skateboard-ui/shadcn/ui/button';
import { Input } from '@stevederico/skateboard-ui/shadcn/ui/input';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@stevederico/skateboard-ui/shadcn/ui/dialog';
import { api } from '../api';
import { installSnippet } from '../lib/snippet';
import type { Site } from '../types';
import { SITES_CHANGED } from '../views/Settings';

interface Props {
  onCreated?: (site: Site) => void;
}

export function AddSiteButton({ onCreated }: Props) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState('');
  const [domain, setDomain] = useState('');
  const [githubRepo, setGithubRepo] = useState('');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [created, setCreated] = useState<Site | null>(null);
  const [copied, setCopied] = useState(false);

  const reset = () => {
    setName('');
    setDomain('');
    setGithubRepo('');
    setErr(null);
    setCreated(null);
    setCopied(false);
  };

  const create = async () => {
    const n = name.trim();
    if (!n) {
      setErr('Name is required');
      return;
    }
    const gh = githubRepo.trim();
    if (gh && !gh.includes('/')) {
      setErr('GitHub repo must be owner/name');
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      const body: { name: string; domain?: string; github_repo?: string } = { name: n };
      const d = domain.trim();
      if (d) body.domain = d;
      if (gh) body.github_repo = gh;
      const site = await api<Site>('/api/sites', {
        method: 'POST',
        body: JSON.stringify(body),
      });
      window.dispatchEvent(new Event(SITES_CHANGED));
      setCreated(site);
      onCreated?.(site);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const snippet = created?.write_key ? installSnippet(created.write_key) : '';

  const finish = () => {
    reset();
    setOpen(false);
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(v) => {
        setOpen(v);
        if (!v) reset();
      }}
    >
      <DialogTrigger
        type="button"
        title="New property"
        aria-label="New property"
        className="shrink-0 border border-border h-7 w-7 inline-flex items-center justify-center font-mono text-sm hover:border-app hover:text-app bg-transparent cursor-pointer"
      >
        +
      </DialogTrigger>
      <DialogContent
        className="sm:max-w-md rounded-none border border-border bg-background shadow-none ring-0 gap-4"
        showCloseButton
      >
        <DialogHeader>
          <DialogTitle className="font-mono uppercase tracking-[0.5px] text-sm">
            {created ? 'Property created' : 'New property'}
          </DialogTitle>
          <DialogDescription className="text-xs text-muted-foreground">
            {created
              ? 'Copy the install snippet into the site’s head, then open Overview anytime for the same code.'
              : 'Web analytics property. Optional GitHub repo adds a Repo tab (downloads/stars/…).'}
          </DialogDescription>
        </DialogHeader>

        {!created ? (
          <>
            <div className="flex flex-col gap-3">
              <div>
                <label className="text-muted-foreground uppercase text-[11px] tracking-[0.5px] mb-1 block" htmlFor="new-site-name">
                  Name
                </label>
                <Input
                  id="new-site-name"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="My Site"
                  autoComplete="off"
                  autoFocus
                  onKeyDown={(e) => { if (e.key === 'Enter') void create(); }}
                />
              </div>
              <div>
                <label className="text-muted-foreground uppercase text-[11px] tracking-[0.5px] mb-1 block" htmlFor="new-site-domain">
                  Domain (optional)
                </label>
                <Input
                  id="new-site-domain"
                  value={domain}
                  onChange={(e) => setDomain(e.target.value)}
                  placeholder="example.com"
                  autoComplete="off"
                  onKeyDown={(e) => { if (e.key === 'Enter') void create(); }}
                />
              </div>
              <div>
                <label className="text-muted-foreground uppercase text-[11px] tracking-[0.5px] mb-1 block" htmlFor="new-site-gh">
                  GitHub repo (optional)
                </label>
                <Input
                  id="new-site-gh"
                  value={githubRepo}
                  onChange={(e) => setGithubRepo(e.target.value)}
                  placeholder="owner/name"
                  autoComplete="off"
                  onKeyDown={(e) => { if (e.key === 'Enter') void create(); }}
                />
              </div>
              {err && <p className="text-bad text-xs">{err}</p>}
            </div>
            <DialogFooter className="gap-2">
              <Button type="button" variant="outline" onClick={finish} disabled={busy}>
                Cancel
              </Button>
              <Button type="button" onClick={() => void create()} disabled={busy || !name.trim()}>
                {busy ? 'Creating…' : 'Create'}
              </Button>
            </DialogFooter>
          </>
        ) : (
          <>
            <div className="flex flex-col gap-2 border border-border p-3">
              <p className="text-xs font-mono uppercase tracking-[0.5px] text-app">{created.name}</p>
              <p className="text-muted-foreground text-xs font-mono break-all">id: {created.id}</p>
              {created.repo && (
                <p className="text-muted-foreground text-xs font-mono break-all">repo: {created.repo}</p>
              )}
              {snippet && (
                <>
                  <pre className="overflow-x-auto border border-app bg-card p-2 text-xs font-mono select-all whitespace-pre-wrap">
                    {snippet}
                  </pre>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    className="self-start"
                    onClick={() => {
                      void navigator.clipboard?.writeText(snippet).then(() => {
                        setCopied(true);
                        setTimeout(() => setCopied(false), 1500);
                      });
                    }}
                  >
                    {copied ? 'Copied' : 'Copy snippet'}
                  </Button>
                </>
              )}
            </div>
            <DialogFooter>
              <Button type="button" onClick={finish}>Done</Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
