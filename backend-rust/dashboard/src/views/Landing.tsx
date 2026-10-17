import { api } from '../api';
import type { LandingEventsResponse, LandingFunnelResponse } from '../types';
import { useAsync, Async } from '../lib/async';
import { Tiles, Section, DataTable, ConvBar } from '../components/ui';
import { fmtNum } from '../lib/format';

export default function Landing() {
  const s = useAsync(() => Promise.all([
    api<LandingEventsResponse>('/api/stats/landing-events'),
    api<LandingFunnelResponse>('/api/stats/landing-funnel'),
  ]), []);
  return (
    <Async state={s}>
      {([ev, fun]) => {
        const top = Math.max(1, ...fun.steps.map((x) => x.count));
        return (
          <>
            <Tiles items={[['Landing events (30d)', fmtNum(ev.total)]]} />

            <Section title="Acquisition funnel (30d)">
              <DataTable
                cols={[
                  { label: 'Step', key: 'step' },
                  { label: 'Count', key: 'count', num: true },
                  { label: 'Conv', num: true, render: (r) => `${Math.round(r.count / top * 100)}%` },
                  { label: '', render: (r) => <ConvBar frac={r.count / top} /> },
                ]}
                rows={fun.steps}
              />
            </Section>

            <Section title="Events by name (30d)">
              {!ev.rows.length ? (
                <p className="text-muted-foreground text-sm">No landing events in window.</p>
              ) : (
                <DataTable
                  cols={[
                    { label: 'Event', key: 'name' },
                    { label: 'Count', key: 'count', num: true },
                    { label: 'Visitors', key: 'visitors', num: true },
                  ]}
                  rows={ev.rows}
                />
              )}
            </Section>
          </>
        );
      }}
    </Async>
  );
}
