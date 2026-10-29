import { useCallback, useState } from 'react';
import { api } from '../api';
import type { Stats, Series, SeriesPoint } from '../types';
import { useAsync, useStream, Async } from '../lib/async';
import { encodeStreamOpts } from '../grpc';
import { StatCards, Section, DataTable } from '../components/ui';
import { SeriesExplorer, type RangeQuery } from '../components/SeriesExplorer';
import { wowGrowth } from '../components/charts';
import { fmtNum, seriesSum, seriesAvg, seriesMax } from '../lib/format';

const METRICS: [string, string][] = [
  ['dau', 'DAU'], ['events', 'Events'], ['errors', 'Errors'], ['signups', 'Signups'], ['pings', 'Pings'],
];

interface RangeSummary {
  activeUsers: number;
  avgDau: number;
  peakDau: number;
  events: number;
  pings: number;
  signups: number;
  errors: number;
  delta: number | null;
}

const fetchMetric = (metric: string, query: string): Promise<SeriesPoint[]> =>
  api<Series>(`/api/stats/series?metric=${metric}&${query}`).then((r) => r.series);

export default function Overview() {
  // Live snapshot over gRPC-Web server streaming (StreamStats): first frame flushes
  // in ~0.3s through Cloudflare, then change-pushes on an interval. Stable deps ([])
  // so it subscribes once — no reconnect churn. The range-scoped KPIs + chart below
  // stay unary (they're a historical picker window, not a live feed).
  const stats = useStream<Stats>('StreamStats', () => encodeStreamOpts(), []);
  const [range, setRange] = useState<RangeQuery>(() => {
    const d = localStorage.getItem('apex_dash_range') || '30';
    return { query: `days=${d}`, label: `${d}d` };
  });
  const onQueryChange = useCallback((q: RangeQuery) => setRange(q), []);
  const [metric, setMetric] = useState(() => localStorage.getItem('apex_dash_metric') || 'dau');
  const pickMetric = useCallback((m: string) => {
    setMetric(m);
    localStorage.setItem('apex_dash_metric', m);
  }, []);

  // KPI cards follow the header range — same series endpoint as the chart.
  const summary = useAsync(async (): Promise<RangeSummary> => {
    const [dau, events, pings, signups, errors, active] = await Promise.all([
      fetchMetric('dau', range.query),
      fetchMetric('events', range.query),
      fetchMetric('pings', range.query),
      fetchMetric('signups', range.query),
      fetchMetric('errors', range.query),
      api<{ active_users: number }>(`/api/stats/active-users?${range.query}`),
    ]);
    return {
      activeUsers: active.active_users,
      avgDau: seriesAvg(dau),
      peakDau: seriesMax(dau),
      events: seriesSum(events),
      pings: seriesSum(pings),
      signups: seriesSum(signups),
      errors: seriesSum(errors),
      delta: wowGrowth(dau),
    };
  }, [range.query]);

  // Top events follow the header range too (range-aware endpoint, not the fixed
  // 24h snapshot in GetStats).
  const topEvents = useAsync(
    () => api<{ rows: { name: string; count: number }[] }>(`/api/stats/top-events?${range.query}`).then((r) => r.rows),
    [range.query],
    { keepPrevious: true },
  );

  const v = (n: number | undefined) => summary.loading || n == null ? '…' : fmtNum(n);

  return (
    <Async state={stats} keepPrevious>
      {(s) => (
        <>
          <StatCards cards={[
            { label: 'Active users', value: v(summary.data?.activeUsers), onClick: () => pickMetric('dau'), active: metric === 'dau' },
            { label: 'Avg DAU', value: v(summary.data?.avgDau), delta: summary.data?.delta, onClick: () => pickMetric('dau'), active: metric === 'dau' },
            { label: 'Peak DAU', value: v(summary.data?.peakDau), onClick: () => pickMetric('dau'), active: metric === 'dau' },
            { label: 'Events', value: v(summary.data?.events), onClick: () => pickMetric('events'), active: metric === 'events' },
            { label: 'Pings', value: v(summary.data?.pings), onClick: () => pickMetric('pings'), active: metric === 'pings' },
            { label: 'Signups', value: v(summary.data?.signups), onClick: () => pickMetric('signups'), active: metric === 'signups' },
            { label: 'Errors', value: v(summary.data?.errors), onClick: () => pickMetric('errors'), active: metric === 'errors' },
          ]} />

          <SeriesExplorer
            metrics={METRICS}
            metric={metric}
            onMetricChange={pickMetric}
            storageKeys={{ metric: 'apex_dash_metric', range: 'apex_dash_range' }}
            onQueryChange={onQueryChange}
            fetchSeries={fetchMetric}
          />

          <Section title={`Top events (${range.label})`}>
            <DataTable
              cols={[{ label: 'Event', key: 'name' }, { label: 'Count', key: 'count', num: true }]}
              rows={topEvents.data ?? []}
            />
          </Section>

          <Section title="Countries">
            <DataTable
              cols={[{ label: 'Country', key: 'country' }, { label: 'Users', key: 'count', num: true }]}
              rows={s.countries}
            />
          </Section>
        </>
      )}
    </Async>
  );
}
