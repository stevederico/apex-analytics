// Repo overview: GitHub downloads / stars / forks / clones / views for one
// repo selected in the main header switcher (`site` prop = owner/name).
// Empty `site` → All Repositories table (growth-chart style).
// Layout: navbar range · clickable metric KPIs → chart → WoW/goal → tables.
import { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { api } from '../api';
import type { SeriesPoint } from '../types';
import { useAsync, Async } from '../lib/async';
import { StatCards, Section, DataTable } from '../components/ui';
import { AreaChart } from '../components/charts';
import { fmtNum, fmtRate } from '../lib/format';
import { shortRepoName } from '../lib/scope';
import { useHeaderSlot } from '../lib/headerSlot';
import AllReposOverview from './AllReposOverview';

type MetricType = 'downloads' | 'stars' | 'forks' | 'views' | 'clones';

const METRICS: [MetricType, string][] = [
  ['downloads', 'Downloads'],
  ['stars', 'Stars'],
  ['forks', 'Forks'],
  ['clones', 'Clones'],
  ['views', 'Views'],
];

interface Snapshot {
  date: string;
  download_count?: number;
  count?: number;
  tag?: string;
}

interface DailyDelta {
  date: string;
  total: number;
}

interface LatestDownloads {
  date: string | null;
  total: number;
  releases: { tag: string; download_count: number }[];
}

interface LatestMetric {
  date: string | null;
  count: number;
  uniques: number;
}

/** `GET /api/metrics/wow` — the server owns the rate math; the UI only plots it. */
interface WowPoint {
  date: string;
  value: number;
  prev_date: string;
  prev_value: number;
  rate: number;
}

interface WowResponse {
  metric: string;
  latest: WowPoint | null;
  points: WowPoint[];
}

function goalFromChart(chart: SeriesPoint[]): { needed: number | null; deadline: string | null } {
  if (chart.length < 2) return { needed: null, deadline: null };
  const now = Date.now();
  const seven = now - 7 * 86_400_000;
  const current = chart[chart.length - 1]!.count;
  let prev = chart[0]!;
  for (const e of chart) {
    const t = new Date(e.date + 'T00:00:00').getTime();
    if (Math.abs(t - seven) < Math.abs(new Date(prev.date + 'T00:00:00').getTime() - seven)) {
      prev = e;
    }
  }
  if (prev.count <= 0) return { needed: null, deadline: null };
  const target = Math.ceil(prev.count * 1.2);
  const needed = Math.max(0, target - current);
  const d = new Date(prev.date + 'T00:00:00');
  d.setDate(d.getDate() + 7);
  return { needed, deadline: d.toISOString().slice(0, 10) };
}

function buildMetricSeries(
  metric: MetricType,
  all: Snapshot[],
  daily: DailyDelta[],
): { chart: SeriesPoint[]; dailyPts: SeriesPoint[] } {
  if (metric === 'downloads') {
    const byDate = new Map<string, number>();
    for (const s of all) {
      byDate.set(s.date, (byDate.get(s.date) ?? 0) + (s.download_count ?? 0));
    }
    const chart = [...byDate.entries()]
      .sort((a, b) => a[0].localeCompare(b[0]))
      .map(([date, count]) => ({ date, count }));
    const dailyPts = daily.map((d) => ({ date: d.date, count: d.total }));
    return { chart, dailyPts };
  }
  const byDate = new Map<string, number>();
  for (const s of all) {
    byDate.set(s.date, (byDate.get(s.date) ?? 0) + (s.count ?? 0));
  }
  const sorted = [...byDate.entries()]
    .sort((a, b) => a[0].localeCompare(b[0]))
    .map(([date, count]) => ({ date, count }));
  if (metric === 'clones' || metric === 'views') {
    const dailyPts = sorted;
    let cum = 0;
    const chart = sorted.map((p) => {
      cum += p.count;
      return { date: p.date, count: cum };
    });
    return { chart, dailyPts };
  }
  return {
    chart: sorted,
    dailyPts: daily.map((d) => ({ date: d.date, count: d.total })),
  };
}

const RANGE_PRESETS: [string, string][] = [
  ['7', '7d'],
  ['30', '30d'],
  ['90', '90d'],
  ['180', '180d'],
  ['all', 'All'],
];

const rangePill = (active: boolean) =>
  `h-8 box-border border px-2.5 text-xs inline-flex items-center ${active ? 'bg-app text-white border-app' : 'border-border text-foreground'}`;
const dateField = 'h-8 box-border bg-background text-foreground border border-input px-2.5 text-sm w-full [color-scheme:dark]';

function utcDaysAgo(days: number): string {
  const d = new Date();
  d.setUTCDate(d.getUTCDate() - days);
  return d.toISOString().slice(0, 10);
}

function filterByRange<T extends { date: string }>(
  rows: T[],
  range: string,
  from: string,
  to: string,
): T[] {
  if (from && to) {
    return rows.filter((r) => r.date >= from && r.date <= to);
  }
  if (range === 'all' || !range) return rows;
  const n = Number(range);
  if (!Number.isFinite(n) || n <= 0) return rows;
  const start = utcDaysAgo(n);
  return rows.filter((r) => r.date >= start);
}

/** `site` is owner/name, or empty string = all-repos overview table. */
export default function Growth({ site: repo }: { site: string }) {
  if (!repo) return <AllReposOverview />;
  return <SingleRepoOverview repo={repo} />;
}

function SingleRepoOverview({ repo }: { repo: string }) {
  const [metric, setMetric] = useState<MetricType>(() => {
    const s = localStorage.getItem('gc_metric');
    return METRICS.some(([k]) => k === s) ? (s as MetricType) : 'downloads';
  });
  const [range, setRange] = useState(() => localStorage.getItem('gc_range') || '30');
  const [from, setFrom] = useState('');
  const [to, setTo] = useState('');
  const [showWow, setShowWow] = useState(() => localStorage.getItem('gc_wow') === '1');
  const [customOpen, setCustomOpen] = useState(false);
  const [draftFrom, setDraftFrom] = useState('');
  const [draftTo, setDraftTo] = useState('');
  const customWrap = useRef<HTMLDivElement>(null);
  const slot = useHeaderSlot();

  const custom = !!(from && to);
  const pickMetric = (m: MetricType) => {
    setMetric(m);
    localStorage.setItem('gc_metric', m);
  };
  const toggleWow = () => {
    setShowWow((v) => {
      localStorage.setItem('gc_wow', v ? '0' : '1');
      return !v;
    });
  };
  const pickRange = (d: string) => {
    setRange(d);
    setFrom('');
    setTo('');
    setCustomOpen(false);
    localStorage.setItem('gc_range', d);
  };

  useEffect(() => {
    if (!customOpen) return;
    const onDoc = (e: MouseEvent) => {
      if (customWrap.current && !customWrap.current.contains(e.target as Node)) {
        setCustomOpen(false);
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setCustomOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDoc);
      document.removeEventListener('keydown', onKey);
    };
  }, [customOpen]);

  const repoQ = `?repo=${encodeURIComponent(repo)}`;

  // Latest totals for every metric KPI (one strip, all clickable).
  const kpis = useAsync(async () => {
    const [dl, stars, forks, clones, views] = await Promise.all([
      api<LatestDownloads>(`/api/downloads/latest${repoQ}`),
      api<LatestMetric>(`/api/metrics/latest?metric=stars&repo=${encodeURIComponent(repo)}`),
      api<LatestMetric>(`/api/metrics/latest?metric=forks&repo=${encodeURIComponent(repo)}`),
      api<LatestMetric>(`/api/metrics/latest?metric=clones&repo=${encodeURIComponent(repo)}`),
      api<LatestMetric>(`/api/metrics/latest?metric=views&repo=${encodeURIComponent(repo)}`),
    ]);
    return {
      downloads: dl.total ?? 0,
      stars: stars.count ?? 0,
      forks: forks.count ?? 0,
      clones: clones.count ?? 0,
      views: views.count ?? 0,
      uniqueClones: clones.uniques ?? 0,
      uniqueViews: views.uniques ?? 0,
    };
  }, [repo, repoQ]);

  // Full series for the selected metric; range applied client-side.
  const series = useAsync(async () => {
    if (metric === 'downloads') {
      const [all, latest, daily] = await Promise.all([
        api<Snapshot[]>(`/api/downloads${repoQ}`),
        api<LatestDownloads>(`/api/downloads/latest${repoQ}`),
        api<DailyDelta[]>(`/api/downloads/daily${repoQ}`),
      ]);
      const { chart, dailyPts } = buildMetricSeries(metric, all, daily);
      return {
        chart,
        dailyPts,
        daily: dailyPts.map((p) => ({ date: p.date, total: p.count })),
        releases: latest.releases ?? [],
      };
    }
    const metricQ = `?metric=${metric}&repo=${encodeURIComponent(repo)}`;
    const [all, daily] = await Promise.all([
      api<Snapshot[]>(`/api/metrics${metricQ}`),
      api<DailyDelta[]>(`/api/metrics/daily${metricQ}`),
    ]);
    const { chart, dailyPts } = buildMetricSeries(metric, all, daily);
    return {
      chart,
      dailyPts,
      daily: dailyPts.map((p) => ({ date: p.date, total: p.count })),
      releases: [] as LatestDownloads['releases'],
    };
  }, [repo, metric, repoQ]);

  const filtered = useMemo(() => {
    const chart = filterByRange(series.data?.chart ?? [], range, from, to);
    const dailyPts = filterByRange(series.data?.dailyPts ?? [], range, from, to);
    const daily = filterByRange(series.data?.daily ?? [], range, from, to);
    return { chart, dailyPts, daily, releases: series.data?.releases ?? [] };
  }, [series.data, range, from, to]);

  // Rate series comes from the API over the FULL history; the range only filters
  // which points are drawn, so even a 7d range keeps its 7-day lookback.
  const wow = useAsync(
    () =>
      api<WowResponse>(
        `/api/metrics/wow?metric=${metric}&repo=${encodeURIComponent(repo)}`,
      ),
    [repo, metric],
  );
  const wowPts = useMemo(
    () =>
      filterByRange(
        (wow.data?.points ?? []).map((p) => ({ date: p.date, count: p.rate })),
        range,
        from,
        to,
      ),
    [wow.data, range, from, to],
  );

  const secondary = useMemo(() => {
    const chart = filtered.chart;
    const daily = filtered.daily;
    const today = new Date().toISOString().slice(0, 10);
    const valueToday = daily.find((d) => d.date === today)?.total ?? 0;
    const wow = wowPts.length ? wowPts[wowPts.length - 1]!.count : null;
    const goal = goalFromChart(chart);
    return { wow, valueToday, ...goal };
  }, [filtered, wowPts]);

  const metricLabel = METRICS.find(([k]) => k === metric)?.[1] ?? metric;
  const title = shortRepoName(repo);
  const totals = kpis.data;
  const rangeTitle = custom
    ? `${from} → ${to}`
    : range === 'all'
      ? 'all time'
      : `last ${range} days`;
  const customBtnLabel = custom ? `${from.slice(5)}→${to.slice(5)}` : 'Custom';

  const rangeControls = (
    <div className="inline-flex gap-1 h-8 items-center flex-wrap">
      {RANGE_PRESETS.map(([d, text]) => (
        <button
          key={d}
          type="button"
          onClick={() => pickRange(d)}
          className={rangePill(range === d && !custom)}
        >
          {text}
        </button>
      ))}
      <div ref={customWrap} className="relative">
        <button
          type="button"
          onClick={() => {
            setDraftFrom(from);
            setDraftTo(to);
            setCustomOpen((o) => !o);
          }}
          className={rangePill(custom)}
          aria-expanded={customOpen}
          aria-haspopup="dialog"
        >
          {customBtnLabel}
        </button>
        {customOpen && (
          <div
            role="dialog"
            aria-label="Custom date range"
            className="absolute right-0 top-full mt-1.5 z-50 border border-border p-3 shadow-md min-w-[220px]"
            style={{ backgroundColor: 'var(--card)', color: 'var(--card-foreground)' }}
          >
            <div className="flex flex-col gap-2">
              <label className="flex flex-col gap-1 text-[11px] uppercase tracking-[0.5px] text-muted-foreground">
                From
                <input
                  type="date"
                  value={draftFrom}
                  onChange={(e) => setDraftFrom(e.target.value)}
                  className={dateField}
                />
              </label>
              <label className="flex flex-col gap-1 text-[11px] uppercase tracking-[0.5px] text-muted-foreground">
                To
                <input
                  type="date"
                  value={draftTo}
                  onChange={(e) => setDraftTo(e.target.value)}
                  className={dateField}
                />
              </label>
              <button
                type="button"
                onClick={() => {
                  if (!draftFrom || !draftTo) return;
                  setFrom(draftFrom);
                  setTo(draftTo);
                  setCustomOpen(false);
                }}
                disabled={!draftFrom || !draftTo}
                className={`h-8 border text-xs ${draftFrom && draftTo ? 'bg-app text-white border-app' : 'border-border text-muted-foreground opacity-50'}`}
              >
                Apply
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );

  return (
    <>
      {slot
        ? createPortal(rangeControls, slot)
        : <div className="mb-2.5 flex justify-end">{rangeControls}</div>}

      <div className="flex flex-wrap gap-2.5 items-center mb-3">
        <span className="font-mono text-sm font-bold uppercase tracking-[0.5px]">{title}</span>
        <span className="text-muted-foreground text-xs font-mono">{repo}</span>
      </div>

      {/* Clickable metric KPIs — select what the chart shows */}
      <StatCards
        cards={METRICS.map(([key, lab]) => ({
          label: lab,
          value: fmtNum(totals?.[key] ?? 0),
          sub:
            key === 'clones' && totals
              ? `(${fmtNum(totals.uniqueClones)} uniq)`
              : key === 'views' && totals
                ? `(${fmtNum(totals.uniqueViews)} uniq)`
                : undefined,
          onClick: () => pickMetric(key),
          active: metric === key,
        }))}
      />

      <Async state={series} keepPrevious>
        {() => (
          <>
            {/* One chart slot: the Week-over-week card swaps the rate chart in here. */}
            <Section
              title={
                showWow
                  ? `${metricLabel} · week over week · ${rangeTitle}`
                  : `${metricLabel} · ${rangeTitle}`
              }
            >
              {showWow ? (
                wowPts.length === 0 ? (
                  <p className="text-muted-foreground text-sm">
                    No rate points in this range. Snapshots before the range are used for the
                    lookback, so short ranges are fine — but the repo needs 8+ days of history
                    (each point compares to 7 days earlier).
                  </p>
                ) : (
                  <AreaChart points={wowPts} percent label={`${metricLabel} week over week`} />
                )
              ) : filtered.chart.length === 0 && filtered.dailyPts.length === 0 ? (
                <p className="text-muted-foreground text-sm">
                  No snapshots in this range. Collectors pull from GitHub on startup and hourly.
                </p>
              ) : (
                <AreaChart
                  points={filtered.chart}
                  daily={filtered.dailyPts}
                  absolute
                  label={metricLabel}
                />
              )}
            </Section>

            <div className="mt-6">
              <h2 className="text-[12px] tracking-[1px] text-muted-foreground mb-2">Growth</h2>
              <StatCards
                cards={[
                  {
                    label: 'Week over week',
                    value: fmtRate(secondary.wow),
                    onClick: toggleWow,
                    active: showWow,
                  },
                  {
                    label: `${metricLabel} today`,
                    value: fmtNum(secondary.valueToday),
                  },
                  {
                    label: 'To 20% WoW goal',
                    value:
                      secondary.needed == null
                        ? '—'
                        : secondary.needed === 0
                          ? 'Hit'
                          : fmtNum(secondary.needed),
                  },
                ]}
              />
            </div>

            {filtered.daily.length > 0 && (
              <Section title="Daily deltas">
                <DataTable
                  cols={[
                    { label: 'Date', key: 'date' },
                    { label: 'Delta', key: 'total', num: true },
                  ]}
                  rows={[...filtered.daily].reverse().slice(0, 30)}
                />
              </Section>
            )}
          </>
        )}
      </Async>
    </>
  );
}
