import { api } from '../api';
import type { LifecycleResponse, LifecycleWeek } from '../types';
import { useAsync, Async } from '../lib/async';
import { Section, DataTable } from '../components/ui';
import { StackedBars } from '../components/charts';

export default function Lifecycle() {
  const s = useAsync(() => api<LifecycleResponse>('/api/stats/lifecycle?weeks=12'), []);
  return (
    <Async state={s}>
      {(d) => (
        <>
          <Section title="Weekly lifecycle">
            <StackedBars rows={d.weeks} />
          </Section>
          <Section title="Detail">
            <DataTable<LifecycleWeek>
              cols={[
                { label: 'Week', key: 'week' },
                { label: 'New', key: 'new', num: true },
                { label: 'Returning', key: 'returning', num: true },
                { label: 'Resurrecting', key: 'resurrecting', num: true },
                { label: 'Dormant', key: 'dormant', num: true },
              ]}
              rows={d.weeks}
            />
          </Section>
        </>
      )}
    </Async>
  );
}
