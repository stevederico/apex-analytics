import { useState } from 'react';
import { api } from '../api';
import type { Issue, IssuesResponse, EventDetailRow, EventDetailsResponse } from '../types';
import { useAsync, Async } from '../lib/async';
import { DataTable, Pill } from '../components/ui';
import { fmtTs } from '../lib/format';
import { SelectField } from '../components/SelectField';
import { Button } from '@stevederico/skateboard-ui/shadcn/ui/button';

/** Resolve/Reopen action with its own busy + error state; parent refetches on success. */
function IssueAction({ issue, onDone }: { issue: Issue; onDone: () => void }) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const resolved = issue.status === 'resolved';
  const click = async () => {
    setBusy(true);
    setErr(null);
    try {
      await api('/api/issues/' + issue.id, {
        method: 'PATCH',
        body: JSON.stringify({ status: resolved ? 'active' : 'resolved' }),
      });
      setBusy(false);
      onDone();
    } catch (e) {
      setBusy(false);
      setErr(e instanceof Error ? e.message : String(e));
    }
  };
  return (
    <>
      <Button variant="outline" size="sm" disabled={busy} onClick={click}>
        {resolved ? 'Reopen' : 'Resolve'}
      </Button>
      {err != null && <div className="text-bad text-xs mt-1">{err}</div>}
    </>
  );
}

/** Pull a human-readable one-liner out of an event's props payload (unknown shape). */
function propsSummary(p: unknown): string {
  if (p == null) return '';
  if (typeof p === 'string') return p;
  if (typeof p === 'object') {
    const o = p as Record<string, unknown>;
    for (const k of ['message', 'error', 'reason', 'detail', 'description', 'code', 'stack']) {
      const v = o[k];
      if (typeof v === 'string' && v) return v;
    }
    try { return JSON.stringify(p); } catch { return String(p); }
  }
  return String(p);
}

/** file/function/line hold either Swift site OR code/reason/provider (structured RSE). */
function issueDetailLine(issue: Issue): string {
  const parts = [issue.file, issue.function, issue.line != null ? String(issue.line) : null]
    .filter((s): s is string => typeof s === 'string' && s.length > 0);
  return parts.join(' · ');
}

/** Drill-down panel for one issue: recent raw occurrences of its error event. */
function IssueDetail({ issue, onClose }: { issue: Issue; onClose: () => void }) {
  const d = useAsync<EventDetailsResponse>(
    () => api<EventDetailsResponse>('/api/stats/event-details?limit=50&name=' + encodeURIComponent(issue.name)),
    [issue.id, issue.name],
  );
  const loc = issueDetailLine(issue);
  return (
    <div className="mt-4 border border-border p-4">
      <div className="flex justify-between items-start gap-4 mb-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <Pill kind={issue.status}>{issue.status}</Pill>
            <span className="font-medium break-all">{issue.name}</span>
          </div>
          {loc && <div className="text-muted-foreground text-xs mt-1 break-all">{loc}</div>}
          <div className="text-muted-foreground text-xs mt-1">
            {issue.count} events · first {fmtTs(issue.first_seen)} · last {fmtTs(issue.last_seen)}
            {issue.last_version ? ` · v${issue.last_version}` : ''}
            {issue.fingerprint ? ' · ' + issue.fingerprint : ''}
          </div>
        </div>
        <Button variant="outline" size="sm" onClick={onClose}>Close</Button>
      </div>
      <Async state={d}>
        {(data) => (
          data.rows.length === 0 ? (
            <p className="text-muted-foreground text-sm">No raw occurrences in the last {data.window_hours}h.</p>
          ) : (
            <>
              <div className="text-muted-foreground text-xs mb-1">
                {data.rows.length} most-recent occurrences (last {data.window_hours}h)
              </div>
              <div className="overflow-x-auto">
                <DataTable<EventDetailRow>
                  cols={[
                    { label: 'When (UTC)', render: (r) => <span className="text-muted-foreground">{r.at}</span>, sortValue: (r) => r.at },
                    { label: 'Version', render: (r) => r.version ?? '—', sortValue: (r) => r.version },
                    { label: 'Install', render: (r) => <span className="text-muted-foreground text-xs">{r.install_id.slice(0, 8)}</span>, sortValue: (r) => r.install_id },
                    { label: 'Details', render: (r) => <span className="text-xs break-all">{propsSummary(r.props)}</span> },
                  ]}
                  rows={data.rows}
                />
              </div>
            </>
          )
        )}
      </Async>
    </div>
  );
}

export default function Issues() {
  const [status, setStatus] = useState('');
  const [selected, setSelected] = useState<Issue | null>(null);
  const [tick, setTick] = useState(0);
  // Unary GetIssues only — StreamIssues first frame (~9KB) stalls mid-transfer
  // behind Cloudflare, so the client never leaves Loading (same hang as 0.97.0).
  const s = useAsync<IssuesResponse>(
    () => api<IssuesResponse>('/api/stats/issues' + (status ? `?status=${encodeURIComponent(status)}` : '')),
    [status, tick],
    { keepPrevious: true },
  );
  const refetch = () => setTick((n) => n + 1);
  return (
    <>
      <div className="flex gap-2.5 items-center mb-2.5">
        <span className="text-muted-foreground text-sm">Filter</span>
        <SelectField
          value={status}
          onChange={setStatus}
          options={[
            { value: '', label: 'all' },
            { value: 'active', label: 'active' },
            { value: 'regressed', label: 'regressed' },
            { value: 'resolved', label: 'resolved' },
          ]}
          aria-label="Issue status filter"
          className="min-w-[8rem]"
        />
      </div>
      <Async state={s} keepPrevious>
        {(d) => (
          d.rows.length === 0 ? (
            <p className="text-muted-foreground text-sm">No issues.</p>
          ) : (
            <DataTable<Issue>
              cols={[
                { label: 'Status', render: (r) => <Pill kind={r.status}>{r.status}</Pill>, sortValue: (r) => r.status },
                { label: 'Error', sortValue: (r) => r.name, render: (r) => (
                  <>
                    <button
                      type="button"
                      className="text-app hover:underline text-left"
                      onClick={() => setSelected((cur) => (cur && cur.id === r.id ? null : r))}
                    >
                      {r.name}
                    </button>
                    <div className="text-muted-foreground text-xs break-all">
                      {issueDetailLine(r)}
                    </div>
                  </>
                ) },
                { label: 'Count', key: 'count', num: true },
                { label: 'Version', render: (r) => r.last_version ?? '—', sortValue: (r) => r.last_version },
                { label: 'First', render: (r) => <span className="text-muted-foreground">{fmtTs(r.first_seen)}</span>, sortValue: (r) => r.first_seen },
                { label: 'Last', render: (r) => <span className="text-muted-foreground">{fmtTs(r.last_seen)}</span>, sortValue: (r) => r.last_seen },
                { label: '', render: (r) => <IssueAction issue={r} onDone={refetch} /> },
              ]}
              rows={d.rows}
            />
          )
        )}
      </Async>
      {selected && <IssueDetail issue={selected} onClose={() => setSelected(null)} />}
    </>
  );
}
