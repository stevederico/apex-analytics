// In-app feedback from the /feedback.js widget. Native scope (site = '') lists
// every site's feedback; a site scope filters to that site. Data is REST
// (`/api/stats/feedback`, no gRPC twin); screenshots load through an authed
// blob fetch because an <img src> can't carry the bearer.
import { useEffect, useState } from 'react';
import { api, apiBlobUrl } from '../api';
import type { FeedbackResponse, FeedbackRow, FeedbackStatus, SitesResponse } from '../types';
import { useAsync, Async } from '../lib/async';
import { DataTable, Pill, Section, Tiles } from '../components/ui';
import { fmtTs } from '../lib/format';
import { feedbackSnippet } from '../lib/snippet';
import { SelectField } from '../components/SelectField';
import { Button } from '@stevederico/skateboard-ui/shadcn/ui/button';

const PILL: Record<FeedbackStatus, 'active' | 'resolved' | undefined> = {
  new: 'active',
  read: undefined,
  archived: 'resolved',
};

/** One-click status change; parent refetches on success. */
function StatusAction({ row, to, label, onDone }: {
  row: FeedbackRow; to: FeedbackStatus; label: string; onDone: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const click = async () => {
    setBusy(true);
    setErr(null);
    try {
      await api('/api/feedback/' + row.id, { method: 'PATCH', body: JSON.stringify({ status: to }) });
      onDone();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <Button variant="outline" size="sm" disabled={busy} onClick={click}>{label}</Button>
      {err != null && <div className="text-bad text-xs mt-1">{err}</div>}
    </>
  );
}

function Screenshot({ id }: { id: number }) {
  const [src, setSrc] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => {
    let url: string | null = null;
    let live = true;
    apiBlobUrl(`/api/feedback/${id}/screenshot`).then(
      (u) => { url = u; if (live) setSrc(u); else URL.revokeObjectURL(u); },
      (e: unknown) => { if (live) setErr(e instanceof Error ? e.message : String(e)); },
    );
    return () => { live = false; if (url) URL.revokeObjectURL(url); };
  }, [id]);
  if (err) return <div className="text-bad text-xs">{err}</div>;
  if (!src) return <p className="text-muted-foreground text-sm">Loading screenshot…</p>;
  return (
    <a href={src} target="_blank" rel="noreferrer">
      <img src={src} alt="Feedback screenshot" className="max-w-full max-h-[32rem] object-contain object-left-top border border-border" />
    </a>
  );
}

function Detail({ row, onClose, onDone }: { row: FeedbackRow; onClose: () => void; onDone: () => void }) {
  const who = [row.end_user_name, row.end_user_email, row.end_user_id].filter(Boolean).join(' · ');
  const meta: [string, string | null][] = [
    ['Site', row.site_name ?? row.site_id],
    ['When', fmtTs(row.created_at)],
    ['User', who || null],
    ['URL', row.url],
    ['App version', row.app_version],
    ['Country', row.country],
    ['User agent', row.user_agent],
  ];
  return (
    <div className="mt-4 border border-border p-4">
      <div className="flex justify-between items-start gap-4 mb-3">
        <div className="flex items-center gap-2">
          <Pill kind={PILL[row.status]}>{row.status}</Pill>
          <span className="text-muted-foreground text-xs">#{row.id}</span>
        </div>
        <div className="flex gap-2">
          {row.status !== 'archived' && <StatusAction row={row} to="archived" label="Archive" onDone={onDone} />}
          {row.status !== 'new' && <StatusAction row={row} to="new" label="Mark new" onDone={onDone} />}
          <Button variant="outline" size="sm" onClick={onClose}>Close</Button>
        </div>
      </div>
      <p className="whitespace-pre-wrap break-words text-sm mb-3">{row.message}</p>
      <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-xs mb-3">
        {meta.filter(([, v]) => v).map(([k, v]) => (
          <div key={k} className="contents">
            <dt className="text-muted-foreground uppercase tracking-[0.5px]">{k}</dt>
            <dd className="break-all">{v}</dd>
          </div>
        ))}
      </dl>
      {row.has_screenshot && <Screenshot id={row.id} />}
    </div>
  );
}

/** Install tag for this site (real write_key when the site list carries it). */
function Install({ site }: { site: string }) {
  const sites = useAsync(
    () => (site ? api<SitesResponse>('/api/sites').then((r) => r.sites) : Promise.resolve([])),
    [site],
  );
  const key = sites.data?.find((s) => s.id === site)?.write_key || 'YOUR_KEY';
  return (
    <Section title="Install widget">
      <p className="text-muted-foreground text-xs mb-2">
        Paste before &lt;/body&gt;. Uses the same write key as the tracker. Optional:
        {' '}<span className="font-mono">DottieFeedback.identify({'{ id, name, email }'})</span>.
      </p>
      <pre className="overflow-x-auto border border-app bg-card p-2 text-xs font-mono select-all whitespace-pre-wrap">
        {feedbackSnippet(key)}
      </pre>
    </Section>
  );
}

export default function Feedback({ site }: { site: string }) {
  const [status, setStatus] = useState('');
  const [selected, setSelected] = useState<number | null>(null);
  const [tick, setTick] = useState(0);
  const params = new URLSearchParams({ all: '1', limit: '500' });
  if (site) params.set('site', site);
  if (status) params.set('status', status);
  const s = useAsync<FeedbackResponse>(
    () => api<FeedbackResponse>('/api/stats/feedback?' + params.toString()),
    [site, status, tick],
    { keepPrevious: true },
  );
  const refetch = () => setTick((n) => n + 1);

  // Opening an unread item marks it read (fire-and-forget; list refreshes after).
  const open = (r: FeedbackRow) => {
    setSelected((cur) => (cur === r.id ? null : r.id));
    if (r.status === 'new') {
      void api('/api/feedback/' + r.id, { method: 'PATCH', body: JSON.stringify({ status: 'read' }) })
        .then(refetch, () => {});
    }
  };

  return (
    <>
      <Async state={s} keepPrevious>
        {(d) => {
          const row = d.rows.find((r) => r.id === selected) ?? null;
          return (
            <>
              <Tiles items={[
                ['Total', d.counts.total],
                ['New', d.counts.new],
                ['Read', d.counts.read],
                ['Archived', d.counts.archived],
              ]} />
              <div className="flex gap-2.5 items-center mb-2.5">
                <span className="text-muted-foreground text-sm">Filter</span>
                <SelectField
                  value={status}
                  onChange={setStatus}
                  options={[
                    { value: '', label: 'all' },
                    { value: 'new', label: 'new' },
                    { value: 'read', label: 'read' },
                    { value: 'archived', label: 'archived' },
                  ]}
                  aria-label="Feedback status filter"
                  className="min-w-[8rem]"
                />
              </div>
              {d.rows.length === 0 ? (
                <p className="text-muted-foreground text-sm">No feedback.</p>
              ) : (
                <DataTable<FeedbackRow>
                  cols={[
                    { label: 'Status', render: (r) => <Pill kind={PILL[r.status]}>{r.status}</Pill>, sortValue: (r) => r.status },
                    { label: 'Message', sortValue: (r) => r.message, render: (r) => (
                      <button
                        type="button"
                        className="text-app hover:underline text-left max-w-[28rem] truncate block"
                        onClick={() => open(r)}
                      >
                        {r.message}
                      </button>
                    ) },
                    ...(site ? [] : [{ label: 'Site', render: (r: FeedbackRow) => r.site_name ?? r.site_id, sortValue: (r: FeedbackRow) => r.site_name }]),
                    { label: 'User', render: (r) => r.end_user_email ?? r.end_user_name ?? '—', sortValue: (r) => r.end_user_email },
                    { label: 'Shot', render: (r) => (r.has_screenshot ? 'yes' : ''), sortValue: (r) => (r.has_screenshot ? 1 : 0) },
                    { label: 'When', render: (r) => <span className="text-muted-foreground">{fmtTs(r.created_at)}</span>, sortValue: (r) => r.created_at },
                  ]}
                  rows={d.rows}
                />
              )}
              {row && <Detail row={row} onClose={() => setSelected(null)} onDone={refetch} />}
            </>
          );
        }}
      </Async>
      <Install site={site} />
    </>
  );
}
