import { useState } from 'react';
import { Card, CardHeader, CardTitle, CardContent } from '@stevederico/skateboard-ui/shadcn/ui/card';
import { Button } from '@stevederico/skateboard-ui/shadcn/ui/button';
import { ThemeToggle } from '../components/ThemeToggle';
import { getKey } from '../api';
import { installSnippet } from '../lib/snippet';
import { VERSION } from '../version';

/** Fired so Shell reloads the site switcher list after create. Kept here since
 *  AddSiteButton (header "+") imports it. */
export const SITES_CHANGED = 'dottie:sites-changed';

/** Sidebar-footer Settings: admin session + generic install reference.
 *  Site creation lives in the header "+" (AddSiteButton); the per-site snippet
 *  with the real key lives in each site's Overview → Install. */
export default function Settings({ onSignOut }: { onSignOut: () => void }) {
  const key = getKey();
  const masked = key ? '•'.repeat(Math.max(0, key.length - 4)) + key.slice(-4) : '—';

  const [copied, setCopied] = useState(false);
  const snippet = installSnippet('YOUR_KEY');

  return (
    <div className="max-w-md flex flex-col gap-6">
      <div>
        <h2 className="text-[12px] tracking-[1px] text-muted-foreground mb-3">Settings</h2>
        <Card>
          <CardHeader>
            <CardTitle>Admin session</CardTitle>
          </CardHeader>
          <CardContent className="flex flex-col gap-4">
            <div>
              <div className="text-muted-foreground uppercase text-[11px] tracking-[0.5px] mb-1">Admin key</div>
              <div className="font-mono text-sm break-all">{masked}</div>
            </div>
            <div className="flex items-center justify-between">
              <div className="text-muted-foreground uppercase text-[11px] tracking-[0.5px]">Theme</div>
              <ThemeToggle />
            </div>
            <div className="flex items-center justify-between">
              <div className="text-muted-foreground uppercase text-[11px] tracking-[0.5px]">Version</div>
              <div className="font-mono text-sm">{VERSION}</div>
            </div>
            <Button variant="outline" onClick={onSignOut} className="w-full">Sign out</Button>
            <p className="text-muted-foreground text-xs">Served by dottie-analytics · /dashboard</p>
          </CardContent>
        </Card>
      </div>

      <div>
        <h2 className="text-[12px] tracking-[1px] text-muted-foreground mb-3">Install</h2>
        <Card>
          <CardHeader>
            <CardTitle>Tracker snippet</CardTitle>
          </CardHeader>
          <CardContent className="flex flex-col gap-3">
            <p className="text-muted-foreground text-xs">
              Paste in a site&apos;s &lt;head&gt;, replacing <span className="font-mono">YOUR_KEY</span> with
              that site&apos;s write key. Add a site with the <span className="font-mono">+</span> in the header;
              its ready-to-copy snippet with the real key is under Overview → Install.
            </p>
            <pre className="overflow-x-auto border border-app bg-card p-2 text-xs font-mono select-all whitespace-pre-wrap">
              {snippet}
            </pre>
            <Button
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
            <p className="text-muted-foreground text-[11px]">
              Visitors can opt out per-site with{' '}
              <span className="font-mono">localStorage.setItem(&apos;dottie.disabled&apos;,&apos;1&apos;)</span>.
            </p>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
