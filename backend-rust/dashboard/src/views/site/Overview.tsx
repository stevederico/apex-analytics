import { useCallback, useState } from 'react';
import { api } from '../../api';
import type {
  Series, SeriesPoint, SiteSummary, SitePagesResponse, SiteReferrersResponse,
  SiteBreakdownResponse,
} from '../../types';
import { useAsync, Async } from '../../lib/async';
import { StatCards, Section, DataTable, TabStrip } from '../../components/ui';
import { SeriesExplorer, type RangeQuery } from '../../components/SeriesExplorer';
import { fmtNum, pct, dur } from '../../lib/format';

const METRICS: [string, string][] = [
  ['visitors', 'Visitors'], ['visits', 'Visits'], ['pageviews', 'Views'], ['events', 'Events'],
];

const PAGE_TABS: [string, string][] = [
  ['path', 'Path'], ['url', 'URL'], ['entry', 'Entry'], ['exit', 'Exit'],
];
const ENV_TABS: [string, string][] = [
  ['browser', 'Browser'], ['os', 'OS'], ['device', 'Device'],
];
const LOC_TABS: [string, string][] = [
  ['country', 'Country'],
];

const fetchMetric = (site: string, metric: string, query: string): Promise<SeriesPoint[]> =>
  api<Series>(`/api/site/series?site=${encodeURIComponent(site)}&metric=${metric}&${query}`)
    .then((r) => r.series);

/** % change cur vs prev; null when undefined. */
function pctDelta(cur: number, prev: number): number | null {
  if (prev <= 0) return cur > 0 ? 100 : cur === 0 ? 0 : null;
  return ((cur - prev) / prev) * 100;
}

/** Bounce rate as percentage points (cur% − prev%). */
function bouncePp(cur: number | null, prev: number | null): number | null {
  if (cur == null || prev == null) return null;
  return (cur - prev) * 100;
}

const EMPTY_SUMMARY: SiteSummary = {
  visitors: 0, visits: 0, views: 0, bounce_rate: null, visit_duration_ms: 0, events: 0,
  prev: { visitors: 0, visits: 0, views: 0, bounce_rate: null, visit_duration_ms: 0, events: 0 },
};

export default function SiteOverview({ site }: { site: string }) {
  const [range, setRange] = useState<RangeQuery>({ query: 'days=30', label: '30d' });
  const onQueryChange = useCallback((q: RangeQuery) => setRange(q), []);
  const [metric, setMetric] = useState(METRICS[0][0]);
  const [pageKind, setPageKind] = useState('path');
  const [envBy, setEnvBy] = useState('browser');
  const [locBy, setLocBy] = useState('country');

  const summary = useAsync<SiteSummary>(
    () => api<SiteSummary>(`/api/site/summary?site=${encodeURIComponent(site)}&${range.query}`),
    [site, range.query],
    { keepPrevious: true },
  );
  // Errors (missing site etc.) fall back to empty KPIs like the old catch.
  const summaryView = summary.error && !summary.data
    ? { loading: false, error: null, data: EMPTY_SUMMARY }
    : summary;

  // Prefetch all page kinds + breakdowns once per range so tab switches are free.
  const pages = useAsync(async () => {
    const kinds = PAGE_TABS.map(([k]) => k);
    const rows = await Promise.all(
      kinds.map((kind) =>
        api<SitePagesResponse>(
          `/api/site/pages?site=${encodeURIComponent(site)}&kind=${kind}&${range.query}`,
        ),
      ),
    );
    return Object.fromEntries(kinds.map((k, i) => [k, rows[i]])) as Record<string, SitePagesResponse>;
  }, [site, range.query]);

  const referrers = useAsync(
    () => api<SiteReferrersResponse>(
      `/api/site/referrers?site=${encodeURIComponent(site)}&${range.query}`,
    ),
    [site, range.query],
  );

  const env = useAsync(async () => {
    const keys = ENV_TABS.map(([k]) => k);
    const rows = await Promise.all(
      keys.map((by) =>
        api<SiteBreakdownResponse>(
          `/api/site/breakdown?site=${encodeURIComponent(site)}&by=${by}&${range.query}`,
        ),
      ),
    );
    return Object.fromEntries(keys.map((k, i) => [k, rows[i]])) as Record<string, SiteBreakdownResponse>;
  }, [site, range.query]);

  const loc = useAsync(async () => {
    const keys = LOC_TABS.map(([k]) => k);
    const rows = await Promise.all(
      keys.map((by) =>
        api<SiteBreakdownResponse>(
          `/api/site/breakdown?site=${encodeURIComponent(site)}&by=${by}&${range.query}`,
        ),
      ),
    );
    return Object.fromEntries(keys.map((k, i) => [k, rows[i]])) as Record<string, SiteBreakdownResponse>;
  }, [site, range.query]);

  return (
    <>
      <Async state={summaryView} keepPrevious>
        {(s) => {
          const p = s.prev;
          return (
            <StatCards cards={[
              {
                label: 'Visitors',
                value: fmtNum(s.visitors),
                delta: pctDelta(s.visitors, p.visitors),
                onClick: () => setMetric('visitors'),
                active: metric === 'visitors',
              },
              {
                label: 'Visits',
                value: fmtNum(s.visits),
                delta: pctDelta(s.visits, p.visits),
                onClick: () => setMetric('visits'),
                active: metric === 'visits',
              },
              {
                label: 'Views',
                value: fmtNum(s.views),
                delta: pctDelta(s.views, p.views),
                onClick: () => setMetric('pageviews'),
                active: metric === 'pageviews',
              },
              {
                label: 'Bounce rate',
                value: pct(s.bounce_rate),
                delta: bouncePp(s.bounce_rate, p.bounce_rate),
                deltaAbs: true,
                reverseDelta: true,
              },
              {
                label: 'Visit duration',
                value: dur(s.visit_duration_ms),
                delta: pctDelta(s.visit_duration_ms, p.visit_duration_ms),
              },
            ]} />
          );
        }}
      </Async>

      <SeriesExplorer
        metrics={METRICS}
        metric={metric}
        onMetricChange={setMetric}
        onQueryChange={onQueryChange}
        fetchSeries={(m, query) => fetchMetric(site, m, query)}
      />

      <Section title="Pages">
        <TabStrip tabs={PAGE_TABS} value={pageKind} onChange={setPageKind} />
        <Async state={pages}>
          {(all) => {
            const data = all[pageKind];
            if (!data?.rows.length) {
              return <p className="text-muted-foreground text-sm">No pageviews in window.</p>;
            }
            return (
              <DataTable
                cols={[
                  { label: 'Page', key: 'url' },
                  { label: 'Views', key: 'views', num: true },
                  { label: 'Visitors', key: 'visitors', num: true },
                ]}
                rows={data.rows}
              />
            );
          }}
        </Async>
      </Section>

      <Section title="Referrers">
        <Async state={referrers}>
          {(data) => !data.rows.length ? (
            <p className="text-muted-foreground text-sm">No referrers in window.</p>
          ) : (
            <DataTable
              cols={[
                { label: 'Referrer', key: 'referrer' },
                { label: 'Visitors', key: 'visitors', num: true },
                { label: 'Hits', key: 'hits', num: true },
              ]}
              rows={data.rows}
            />
          )}
        </Async>
      </Section>

      <Section title="Environment">
        <TabStrip tabs={ENV_TABS} value={envBy} onChange={setEnvBy} />
        <Async state={env}>
          {(all) => {
            const data = all[envBy];
            if (!data?.rows.length) {
              return <p className="text-muted-foreground text-sm">No data in window.</p>;
            }
            return (
              <DataTable
                cols={[
                  { label: 'Name', key: 'name' },
                  { label: 'Visitors', key: 'visitors', num: true },
                  { label: 'Hits', key: 'hits', num: true },
                ]}
                rows={data.rows}
              />
            );
          }}
        </Async>
      </Section>

      <Section title="Location">
        <TabStrip tabs={LOC_TABS} value={locBy} onChange={setLocBy} />
        <Async state={loc}>
          {(all) => {
            const data = all[locBy];
            if (!data?.rows.length) {
              return <p className="text-muted-foreground text-sm">No data in window.</p>;
            }
            return (
              <DataTable
                cols={[
                  { label: 'Name', key: 'name' },
                  { label: 'Visitors', key: 'visitors', num: true },
                  { label: 'Hits', key: 'hits', num: true },
                ]}
                rows={data.rows}
              />
            );
          }}
        </Async>
      </Section>
    </>
  );
}
