import { useCallback, useState } from 'react';
import { api } from '../../api';
import type { Series, SeriesPoint, SiteEventsResponse } from '../../types';
import { useAsync, Async } from '../../lib/async';
import { Section, DataTable } from '../../components/ui';
import { SeriesExplorer, type RangeQuery } from '../../components/SeriesExplorer';
import { fmtNum } from '../../lib/format';

const METRICS: [string, string][] = [['events', 'Events']];

/** Map series range query → hours for /api/site/events (clamped [1, 8760]). */
function rangeToHours(query: string): number {
  const days = /(?:^|&)days=(\d+)/.exec(query);
  if (days) return Math.min(8760, Math.max(1, Number(days[1]) * 24));
  const from = /(?:^|&)from=([^&]+)/.exec(query)?.[1];
  const to = /(?:^|&)to=([^&]+)/.exec(query)?.[1];
  if (from && to) {
    const a = Date.parse(from + 'T00:00:00Z');
    const b = Date.parse(to + 'T00:00:00Z');
    if (Number.isFinite(a) && Number.isFinite(b)) {
      const ms = Math.abs(b - a) + 86_400_000;
      return Math.min(8760, Math.max(1, Math.ceil(ms / 3_600_000)));
    }
  }
  return 720;
}

export default function SiteEvents({ site }: { site: string }) {
  const [range, setRange] = useState<RangeQuery>({ query: 'days=30', label: '30d' });
  const onQueryChange = useCallback((q: RangeQuery) => setRange(q), []);
  const hours = rangeToHours(range.query);

  const table = useAsync(
    () => api<SiteEventsResponse>(
      `/api/site/events?site=${encodeURIComponent(site)}&hours=${hours}`,
    ),
    [site, hours],
  );

  return (
    <>
      <SeriesExplorer
        metrics={METRICS}
        onQueryChange={onQueryChange}
        fetchSeries={(metric, query) =>
          api<Series>(`/api/site/series?site=${encodeURIComponent(site)}&metric=${metric}&${query}`)
            .then((r: Series) => r.series as SeriesPoint[])}
      />

      <Async state={table}>
        {(data) => (
          <Section title={`Events · ${fmtNum(data.total)} total`}>
            {!data.rows.length ? (
              <p className="text-muted-foreground text-sm">No custom events in window.</p>
            ) : (
              <DataTable
                cols={[
                  { label: 'Event', key: 'name' },
                  { label: 'Count', key: 'count', num: true },
                  { label: 'Visitors', key: 'visitors', num: true },
                ]}
                rows={data.rows}
              />
            )}
          </Section>
        )}
      </Async>
    </>
  );
}
