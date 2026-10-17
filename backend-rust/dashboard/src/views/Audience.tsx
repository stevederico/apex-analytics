// Audience page: acquisition → activation → retention → segments, on one tab.
// A shared navbar picker (All / presets / custom) drives Attribution, Activation
// and Retention; Breakdown is an all-time demographic segment (its own selector).
import { useState } from 'react';
import { api } from '../api';
import type {
  ActivationResponse, AttributionResponse, RetentionResponse,
  BreakdownResponse, BreakdownRow,
} from '../types';
import { useAsync, Async } from '../lib/async';
import { Tiles, Section, DataTable, Pill } from '../components/ui';
import { SelectField } from '../components/SelectField';
import { dur, pct } from '../lib/format';
import { RangePicker, daysWindow, type Window } from '../components/RangePicker';

function Attribution({ win }: { win: Window }) {
  const s = useAsync(
    () => api<AttributionResponse>('/api/stats/attribution?' + win.params),
    [win.params],
    { keepPrevious: true },
  );
  return (
    <Async state={s} keepPrevious>
      {(d) => (
        <>
          <Tiles items={[[`Signups (${win.label})`, d.total]]} />
          <Section title="By campaign">
            <DataTable
              cols={[
                { label: 'Source', key: 'source' },
                { label: 'Medium', key: 'medium' },
                { label: 'Campaign', key: 'campaign' },
                { label: 'Signups', key: 'signups', num: true },
              ]}
              rows={d.rows}
            />
          </Section>
        </>
      )}
    </Async>
  );
}

function Activation({ win }: { win: Window }) {
  const s = useAsync(
    () => api<ActivationResponse>('/api/stats/activation?' + win.params),
    [win.params],
    { keepPrevious: true },
  );
  return (
    <Async state={s} keepPrevious>
      {(d) => (
        <>
          <Tiles items={[
            [`Signups (${win.label})`, d.total],
            ['Activated', d.activated],
            ['Rate', d.activation_rate == null ? '—' : Math.round(d.activation_rate * 100) + '%'],
          ]} />
          <Section title="Recent signups">
            <DataTable
              cols={[
                { label: 'Name', key: 'name' },
                { label: 'Email', key: 'email' },
                { label: 'Ver', key: 'version' },
                { label: 'Country', key: 'country' },
                { label: 'Activated', sortValue: (r) => r.activated ? 1 : 0, render: (r) => r.activated ? <Pill kind="resolved">yes</Pill> : <Pill kind="active">no</Pill> },
                { label: 'TTA', sortValue: (r) => r.time_to_activation_ms, render: (r) => dur(r.time_to_activation_ms) },
                { label: 'App time', sortValue: (r) => r.total_session_ms, render: (r) => dur(r.total_session_ms) },
                { label: 'Sessions', key: 'session_count', num: true },
              ]}
              rows={d.users}
            />
          </Section>
        </>
      )}
    </Async>
  );
}

function Retention({ win }: { win: Window }) {
  const s = useAsync(
    () => api<RetentionResponse>('/api/stats/retention?' + win.params),
    [win.params],
    { keepPrevious: true },
  );
  return (
    <Async state={s} keepPrevious>
      {(d) => (
        <Section title="Weekly cohort retention">
          <DataTable
            cols={[
              { label: 'Cohort week', key: 'week' },
              { label: 'Size', key: 'size', num: true },
              { label: 'Engaged', key: 'engaged', num: true },
              { label: 'D1', num: true, sortValue: (r) => r.d1?.rate, render: (r) => pct(r.d1?.rate) },
              { label: 'D7', num: true, sortValue: (r) => r.d7?.rate, render: (r) => pct(r.d7?.rate) },
              { label: 'D30', num: true, sortValue: (r) => r.d30?.rate, render: (r) => pct(r.d30?.rate) },
            ]}
            rows={[...d.cohorts].reverse()}
          />
        </Section>
      )}
    </Async>
  );
}

/** All-time user segments (version / os / country) — not range-driven. */
function Breakdown() {
  const [by, setBy] = useState('version');
  const s = useAsync(() => api<BreakdownResponse>('/api/stats/breakdown?by=' + by), [by]);
  return (
    <Section title="Segments (all-time)">
      <div className="flex gap-2.5 items-center mb-2.5">
        <span className="text-muted-foreground text-sm">By</span>
        <SelectField
          value={by}
          onChange={setBy}
          options={[
            { value: 'version', label: 'version' },
            { value: 'os', label: 'os' },
            { value: 'country', label: 'country' },
          ]}
          aria-label="Breakdown dimension"
          className="min-w-[7rem]"
        />
      </div>
      <Async state={s} keepPrevious>
        {(d) => (
          <DataTable<BreakdownRow>
            cols={[
              { label: by, key: 'bucket' },
              { label: 'Users', key: 'users', num: true },
              { label: 'Engaged', key: 'engaged', num: true },
            ]}
            rows={d.rows}
          />
        )}
      </Async>
    </Section>
  );
}

export default function Audience() {
  const [win, setWin] = useState<Window>(() => daysWindow(30));
  return (
    <>
      <RangePicker value={win} onChange={setWin} />
      <Attribution win={win} />
      <Activation win={win} />
      <Retention win={win} />
      <Breakdown />
    </>
  );
}
