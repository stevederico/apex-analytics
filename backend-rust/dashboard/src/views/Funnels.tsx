import { useState } from 'react';
import { api } from '../api';
import type { ActivationResponse, FunnelResponse } from '../types';
import { useAsync, Async } from '../lib/async';
import { Tiles, Section, DataTable, ConvBar } from '../components/ui';
import { fmtNum } from '../lib/format';
import { RangePicker, daysWindow, type Window } from '../components/RangePicker';

export default function Funnels() {
  const [win, setWin] = useState<Window>(() => daysWindow(7));
  const s = useAsync(
    () => Promise.all([
      api<ActivationResponse>('/api/stats/activation?' + win.params),
      api<FunnelResponse>('/api/stats/funnel?' + win.params),
    ]),
    [win.params],
    { keepPrevious: true },
  );
  return (
    <>
      <RangePicker value={win} onChange={setWin} />
      <Async state={s} keepPrevious>
        {([act, fun]) => {
          const top = Math.max(1, ...fun.steps.map((x) => x.installs));
          return (
            <>
              <Tiles items={[
                [`Signups (${win.label})`, fmtNum(act.total)],
                ['Activated', fmtNum(act.activated)],
                ['Rate', act.activation_rate == null ? '—' : Math.round(act.activation_rate * 100) + '%'],
              ]} />
              <Section title={`Funnel steps (${win.label})`}>
                {fun.steps.length === 0 ? (
                  <p className="text-muted-foreground text-sm">No funnel.* events in window.</p>
                ) : (
                  <DataTable
                    cols={[
                      { label: 'Step', key: 'name' },
                      { label: 'Installs', key: 'installs', num: true },
                      { label: 'Conv', num: true, render: (r) => `${Math.round((r.installs / top) * 100)}%` },
                      { label: '', render: (r) => <ConvBar frac={r.installs / top} /> },
                    ]}
                    rows={fun.steps}
                  />
                )}
              </Section>
            </>
          );
        }}
      </Async>
    </>
  );
}
