// Growth-chart style “All Repositories” table: per-repo downloads/stars/forks/
// clones/views with Daily | Total toggle, sortable columns, totals footer.
import { useMemo, useState } from 'react';
import { api } from '../api';
import { useAsync, Async } from '../lib/async';
import { Panel } from '../components/ui';
import { shortRepoName, useScopeNav } from '../lib/scope';
import { fmtNum } from '../lib/format';
import {
  Table, TableHeader, TableBody, TableRow, TableHead, TableCell,
} from '@stevederico/skateboard-ui/shadcn/ui/table';

type Mode = 'total' | 'daily';
type SortKey =
  | 'repo'
  | 'downloads'
  | 'stars'
  | 'forks'
  | 'clones'
  | 'uniqueClones'
  | 'views'
  | 'uniqueViews';

interface Row {
  repo: string;
  downloads: number;
  stars: number;
  forks: number;
  clones: number;
  uniqueClones: number;
  views: number;
  uniqueViews: number;
}

interface DlRow {
  repo: string;
  date: string;
  tag: string;
  download_count: number;
}

interface MetricRow {
  repo: string;
  date: string;
  metric: string;
  count: number;
  uniques: number;
}

const COLS: { key: SortKey; label: string }[] = [
  { key: 'repo', label: 'Repo' },
  { key: 'downloads', label: 'Downloads' },
  { key: 'stars', label: 'Stars' },
  { key: 'forks', label: 'Forks' },
  { key: 'clones', label: 'Clones' },
  { key: 'uniqueClones', label: 'Unique Cloners' },
  { key: 'views', label: 'Views' },
  { key: 'uniqueViews', label: 'Unique Views' },
];

function emptyRow(repo: string): Row {
  return {
    repo,
    downloads: 0,
    stars: 0,
    forks: 0,
    clones: 0,
    uniqueClones: 0,
    views: 0,
    uniqueViews: 0,
  };
}

/** Latest cumulative downloads per repo (sum tags on max date). */
function latestDownloads(rows: DlRow[]): Map<string, number> {
  const byRepoDate = new Map<string, Map<string, number>>();
  for (const r of rows) {
    let dates = byRepoDate.get(r.repo);
    if (!dates) {
      dates = new Map();
      byRepoDate.set(r.repo, dates);
    }
    dates.set(r.date, (dates.get(r.date) ?? 0) + (r.download_count ?? 0));
  }
  const out = new Map<string, number>();
  for (const [repo, dates] of byRepoDate) {
    const maxDate = [...dates.keys()].sort().at(-1);
    if (maxDate) out.set(repo, dates.get(maxDate) ?? 0);
  }
  return out;
}

/** Day-over-day download deltas summed for the most recent day with any delta. */
function dailyDownloads(rows: DlRow[]): Map<string, number> {
  // Per repo: sort dates, take last two cumulative totals → delta
  const byRepoDate = new Map<string, Map<string, number>>();
  for (const r of rows) {
    let dates = byRepoDate.get(r.repo);
    if (!dates) {
      dates = new Map();
      byRepoDate.set(r.repo, dates);
    }
    dates.set(r.date, (dates.get(r.date) ?? 0) + (r.download_count ?? 0));
  }
  const out = new Map<string, number>();
  for (const [repo, dates] of byRepoDate) {
    const sorted = [...dates.keys()].sort();
    if (sorted.length < 2) {
      out.set(repo, 0);
      continue;
    }
    const last = sorted[sorted.length - 1]!;
    const prev = sorted[sorted.length - 2]!;
    out.set(repo, Math.max(0, (dates.get(last) ?? 0) - (dates.get(prev) ?? 0)));
  }
  return out;
}

/** Latest (or daily) stars/forks; sum (or latest day) for clones/views. */
function metricAgg(
  rows: MetricRow[],
  metric: string,
  mode: Mode,
): { count: Map<string, number>; uniques: Map<string, number> } {
  const filtered = rows.filter((r) => r.metric === metric);
  const count = new Map<string, number>();
  const uniques = new Map<string, number>();

  if (metric === 'stars' || metric === 'forks') {
    // Cumulative: take value on max date per repo. Daily: max - prev date.
    const byRepo = new Map<string, { date: string; count: number }[]>();
    for (const r of filtered) {
      const list = byRepo.get(r.repo) ?? [];
      list.push({ date: r.date, count: r.count ?? 0 });
      byRepo.set(r.repo, list);
    }
    for (const [repo, list] of byRepo) {
      list.sort((a, b) => a.date.localeCompare(b.date));
      if (mode === 'total' || list.length === 1) {
        count.set(repo, list[list.length - 1]!.count);
      } else {
        const last = list[list.length - 1]!;
        const prev = list[list.length - 2]!;
        count.set(repo, Math.max(0, last.count - prev.count));
      }
      uniques.set(repo, 0);
    }
    return { count, uniques };
  }

  // clones / views: total = sum all days; daily = last day only
  const byRepoDate = new Map<string, Map<string, { c: number; u: number }>>();
  for (const r of filtered) {
    let dates = byRepoDate.get(r.repo);
    if (!dates) {
      dates = new Map();
      byRepoDate.set(r.repo, dates);
    }
    dates.set(r.date, {
      c: (dates.get(r.date)?.c ?? 0) + (r.count ?? 0),
      u: (dates.get(r.date)?.u ?? 0) + (r.uniques ?? 0),
    });
  }
  for (const [repo, dates] of byRepoDate) {
    const sorted = [...dates.keys()].sort();
    if (mode === 'daily') {
      const last = sorted[sorted.length - 1]!;
      const v = dates.get(last)!;
      count.set(repo, v.c);
      uniques.set(repo, v.u);
    } else {
      let c = 0;
      let u = 0;
      for (const v of dates.values()) {
        c += v.c;
        u += v.u;
      }
      count.set(repo, c);
      uniques.set(repo, u);
    }
  }
  return { count, uniques };
}

export default function AllReposOverview() {
  const nav = useScopeNav();
  const [mode, setMode] = useState<Mode>('total');
  const [sortKey, setSortKey] = useState<SortKey>('downloads');
  const [sortDir, setSortDir] = useState<'asc' | 'desc'>('desc');

  const state = useAsync(async () => {
    const [reposRes, downloads, stars, forks, clones, views] = await Promise.all([
      api<{ repos: { repo: string }[] }>('/api/repos'),
      api<DlRow[]>('/api/downloads'),
      api<MetricRow[]>('/api/metrics?metric=stars'),
      api<MetricRow[]>('/api/metrics?metric=forks'),
      api<MetricRow[]>('/api/metrics?metric=clones'),
      api<MetricRow[]>('/api/metrics?metric=views'),
    ]);
    const repoNames = (reposRes.repos ?? []).map((r) =>
      typeof r === 'string' ? r : String((r as { repo: string }).repo),
    );
    const allMetrics = [...stars, ...forks, ...clones, ...views];
    // Tag metric field if missing from response shape
    const withMetric = (rows: MetricRow[], m: string) =>
      rows.map((r) => ({ ...r, metric: r.metric || m }));

    const dlMap =
      mode === 'total' ? latestDownloads(downloads) : dailyDownloads(downloads);
    const st = metricAgg(withMetric(stars, 'stars'), 'stars', mode);
    const fk = metricAgg(withMetric(forks, 'forks'), 'forks', mode);
    const cl = metricAgg(withMetric(clones, 'clones'), 'clones', mode);
    const vw = metricAgg(withMetric(views, 'views'), 'views', mode);

    // Union of every repo seen in list or data
    const set = new Set<string>(repoNames);
    for (const r of downloads) set.add(r.repo);
    for (const r of allMetrics) set.add(r.repo);

    const rows: Row[] = [...set].map((repo) => ({
      repo,
      downloads: dlMap.get(repo) ?? 0,
      stars: st.count.get(repo) ?? 0,
      forks: fk.count.get(repo) ?? 0,
      clones: cl.count.get(repo) ?? 0,
      uniqueClones: cl.uniques.get(repo) ?? 0,
      views: vw.count.get(repo) ?? 0,
      uniqueViews: vw.uniques.get(repo) ?? 0,
    }));
    return rows;
  }, [mode]);

  const sorted = useMemo(() => {
    const rows = state.data ?? [];
    return [...rows].sort((a, b) => {
      if (sortKey === 'repo') {
        const cmp = shortRepoName(a.repo).localeCompare(shortRepoName(b.repo));
        return sortDir === 'asc' ? cmp : -cmp;
      }
      const av = a[sortKey] ?? 0;
      const bv = b[sortKey] ?? 0;
      return sortDir === 'asc' ? av - bv : bv - av;
    });
  }, [state.data, sortKey, sortDir]);

  const totals = useMemo(() => {
    const z = emptyRow('Total');
    for (const r of state.data ?? []) {
      z.downloads += r.downloads;
      z.stars += r.stars;
      z.forks += r.forks;
      z.clones += r.clones;
      z.uniqueClones += r.uniqueClones;
      z.views += r.views;
      z.uniqueViews += r.uniqueViews;
    }
    return z;
  }, [state.data]);

  const handleSort = (key: SortKey) => {
    if (sortKey === key) setSortDir((d) => (d === 'asc' ? 'desc' : 'asc'));
    else {
      setSortKey(key);
      setSortDir(key === 'repo' ? 'asc' : 'desc');
    }
  };

  const modeBtn = (m: Mode, label: string) => (
    <button
      type="button"
      onClick={() => setMode(m)}
      className={`h-8 box-border border px-3 text-xs ${
        mode === m ? 'bg-app text-white border-app' : 'border-border text-foreground'
      }`}
    >
      {label}
    </button>
  );

  return (
    <Async state={state}>
      {() => (
        <Panel>
          <div className="flex items-start justify-between gap-3 mb-4 flex-wrap">
            <div>
              <h2 className="text-base font-semibold tracking-tight">All Repositories</h2>
              <p className="text-muted-foreground text-sm mt-0.5">
                {mode === 'daily'
                  ? 'Most recent day across all tracked repos'
                  : 'All-time totals across all tracked repos'}
              </p>
            </div>
            <div className="inline-flex gap-0 border border-border">
              {modeBtn('daily', 'Daily')}
              {modeBtn('total', 'Total')}
            </div>
          </div>

          <Table>
            <TableHeader>
              <TableRow className="border-border hover:bg-transparent">
                {COLS.map(({ key, label }) => (
                  <TableHead
                    key={key}
                    className={`text-muted-foreground uppercase text-[11px] tracking-[0.5px] cursor-pointer select-none ${
                      key !== 'repo' ? 'text-right' : ''
                    }`}
                    onClick={() => handleSort(key)}
                  >
                    <span className={`inline-flex items-center gap-1 ${key !== 'repo' ? 'justify-end w-full' : ''}`}>
                      {label}
                      {sortKey === key && (
                        <span className="text-app">{sortDir === 'asc' ? '↑' : '↓'}</span>
                      )}
                    </span>
                  </TableHead>
                ))}
              </TableRow>
            </TableHeader>
            <TableBody>
              {sorted.map((row) => (
                <TableRow key={row.repo} className="border-border">
                  <TableCell>
                    <button
                      type="button"
                      className="font-medium hover:underline text-left"
                      onClick={() => nav({ kind: 'repo', id: row.repo })}
                    >
                      {shortRepoName(row.repo)}
                    </button>
                  </TableCell>
                  <TableCell className="text-right tabular-nums">{fmtNum(row.downloads)}</TableCell>
                  <TableCell className="text-right tabular-nums">{fmtNum(row.stars)}</TableCell>
                  <TableCell className="text-right tabular-nums">{fmtNum(row.forks)}</TableCell>
                  <TableCell className="text-right tabular-nums">{fmtNum(row.clones)}</TableCell>
                  <TableCell className="text-right tabular-nums">{fmtNum(row.uniqueClones)}</TableCell>
                  <TableCell className="text-right tabular-nums">{fmtNum(row.views)}</TableCell>
                  <TableCell className="text-right tabular-nums">{fmtNum(row.uniqueViews)}</TableCell>
                </TableRow>
              ))}
              <TableRow className="border-border font-semibold border-t-2">
                <TableCell>Total</TableCell>
                <TableCell className="text-right tabular-nums">{fmtNum(totals.downloads)}</TableCell>
                <TableCell className="text-right tabular-nums">{fmtNum(totals.stars)}</TableCell>
                <TableCell className="text-right tabular-nums">{fmtNum(totals.forks)}</TableCell>
                <TableCell className="text-right tabular-nums">{fmtNum(totals.clones)}</TableCell>
                <TableCell className="text-right tabular-nums">{fmtNum(totals.uniqueClones)}</TableCell>
                <TableCell className="text-right tabular-nums">{fmtNum(totals.views)}</TableCell>
                <TableCell className="text-right tabular-nums">{fmtNum(totals.uniqueViews)}</TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </Panel>
      )}
    </Async>
  );
}
