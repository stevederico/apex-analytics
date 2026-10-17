// Metric selector + shared range picker + area chart. Used by the Dottie
// Overview and each per-site Overview so the picker is identical everywhere
// (All · 24h · 7d · 30d · 90d · 180d · Custom). Unary fetch by design: this is a
// historical window tool (the past doesn't change), and the picker's 24h/all
// windows don't ride the current gRPC stream messages. Live push is used for the
// current-state snapshots instead (Overview StreamStats, Issues StreamIssues).
import { useEffect, useState } from 'react';
import { createPortal } from 'react-dom';
import type { SeriesPoint } from '../types';
import { AreaChart } from './charts';
import { useAsync, Async } from '../lib/async';
import { useHeaderSlot } from '../lib/headerSlot';
import { Section } from './ui';
import { SelectField } from './SelectField';
import { RangePicker, daysWindow, type Window } from './RangePicker';

export interface RangeQuery {
  query: string;
  label: string;
}

interface Props {
  metrics: [string, string][];
  fetchSeries: (metric: string, query: string) => Promise<SeriesPoint[]>;
  storageKeys?: { metric: string; range: string };
  /** Fires on mount + whenever the range changes so parent KPI cards can follow. */
  onQueryChange?: (q: RangeQuery) => void;
  /** Controlled metric (e.g. from KPI click). When set, parent owns the value. */
  metric?: string;
  onMetricChange?: (metric: string) => void;
}

function loadWindow(key: string | undefined): Window {
  if (key) {
    try {
      const raw = localStorage.getItem(key);
      if (raw) {
        const w = JSON.parse(raw) as Window;
        if (w && typeof w.params === 'string' && typeof w.label === 'string') return w;
      }
    } catch { /* fall through to default */ }
  }
  return daysWindow(30);
}

export function SeriesExplorer({
  metrics, fetchSeries, storageKeys, onQueryChange, metric: metricProp, onMetricChange,
}: Props) {
  const [metricInner, setMetricInner] = useState(
    () => metricProp || (storageKeys && localStorage.getItem(storageKeys.metric)) || metrics[0][0],
  );
  const metric = metricProp ?? metricInner;
  const [win, setWin] = useState<Window>(() => loadWindow(storageKeys?.range));

  const label = (metrics.find((m) => m[0] === metric) ?? ['', metric])[1];

  // Keep prior series painted while the next metric/range loads (no chart flash).
  const state = useAsync(() => fetchSeries(metric, win.params), [metric, win.params], { keepPrevious: true });
  const slot = useHeaderSlot();

  useEffect(() => {
    onQueryChange?.({ query: win.params, label: win.label });
  }, [win, onQueryChange]);

  // Sync when parent drives metric (KPI click).
  useEffect(() => {
    if (metricProp != null) setMetricInner(metricProp);
  }, [metricProp]);

  const pickMetric = (v: string) => {
    setMetricInner(v);
    onMetricChange?.(v);
    if (storageKeys) localStorage.setItem(storageKeys.metric, v);
  };
  const pickWindow = (w: Window) => {
    setWin(w);
    if (storageKeys) localStorage.setItem(storageKeys.range, JSON.stringify(w));
  };

  const metricControl = (
    <SelectField
      value={metric}
      onChange={pickMetric}
      options={metrics.map(([v, l]) => ({ value: v, label: l }))}
      aria-label="Metric"
      className="min-w-[7.5rem]"
    />
  );

  return (
    <>
      {slot ? createPortal(metricControl, slot) : <div className="mb-2.5">{metricControl}</div>}
      <RangePicker value={win} onChange={pickWindow} />
      <Async state={state} keepPrevious>
        {(series) => (
          <div className={state.loading ? 'opacity-70 transition-opacity' : 'transition-opacity'}>
            <Section title={`${label} — ${win.label}`} />
            <AreaChart points={series} label={label.toLowerCase()} />
          </div>
        )}
      </Async>
    </>
  );
}
