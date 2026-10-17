// Hand-rolled inline-SVG charts (ported from the legacy vanilla dashboard).
// Colors come from CSS tokens (--color-app is the brutalist orange), so charts
// track the active light/dark theme automatically.
import { useEffect, useRef, useState, type RefObject } from 'react';
import type { SeriesPoint, LifecycleWeek } from '../types';
import { fmtNum, fmtBucket, fmtRate } from '../lib/format';

/**
 * Measure an element's width with a ResizeObserver, loop-safe.
 *
 * A chart whose HEIGHT grows with its measured WIDTH can feedback-loop: taller
 * chart → page overflows → vertical scrollbar appears → content width shrinks →
 * RO fires → width state changes → height changes → scrollbar toggles back →
 * width grows → … This "rapid-fire re-render" only shows with DevTools closed
 * (docked DevTools changes the layout so the scrollbar stops toggling). Two
 * guards break it: defer the state write to the next frame (rAF, so the write
 * lands outside the observer's own layout pass) and skip no-op width updates.
 * The real cure is `scrollbar-gutter: stable` on the scroll container (so the
 * scrollbar never toggles the width); this keeps the chart robust regardless.
 */
function useElementWidth(fallback = 800): [number, RefObject<HTMLDivElement | null>] {
  const ref = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(fallback);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    let raf = 0;
    const ro = new ResizeObserver((entries) => {
      const nw = entries[0]?.contentRect.width;
      if (!nw || nw <= 0) return;
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => {
        const rounded = Math.round(nw);
        setW((prev) => (prev === rounded ? prev : rounded));
      });
    });
    ro.observe(el);
    setW(Math.round(el.clientWidth) || fallback);
    return () => { cancelAnimationFrame(raf); ro.disconnect(); };
  }, [fallback]);
  return [w, ref];
}

export function Sparkline({ values }: { values: number[] }) {
  if (!values.length) return <div className="text-muted-foreground text-xs">no data</div>;
  const max = Math.max(1, ...values), w = 600, h = 60, step = w / Math.max(1, values.length - 1);
  const pts = values.map((v, i) => `${(i * step).toFixed(1)},${(h - (v / max) * (h - 4) - 2).toFixed(1)}`).join(' ');
  return (
    <svg className="block w-full h-[60px]" viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none">
      <polyline fill="none" stroke="var(--color-app)" strokeWidth={1.5} points={pts} />
    </svg>
  );
}

/** [{date,count}] daily → cumulative running total (for the Total toggle). */
function toCumulative(pts: SeriesPoint[]): SeriesPoint[] {
  let t = 0;
  return pts.map((p) => ({ date: p.date, count: (t += p.count) }));
}

/** Absolute/cumulative series → day-over-day deltas (growth-chart Daily mode). */
function toDeltas(abs: SeriesPoint[]): SeriesPoint[] {
  const out: SeriesPoint[] = [];
  for (let i = 1; i < abs.length; i++) {
    const prev = abs[i - 1]!;
    const cur = abs[i]!;
    out.push({ date: cur.date, count: cur.count - prev.count });
  }
  return out;
}

/** Week-over-week % on a daily series: last point vs the point ~7 rows back. */
export function wowGrowth(pts: SeriesPoint[] | undefined): number | null {
  if (!pts || pts.length < 2) return null;
  const last = pts[pts.length - 1].count;
  const prev = pts[Math.max(0, pts.length - 1 - 7)].count;
  if (prev <= 0) return null;
  return ((last - prev) / prev) * 100;
}

interface Hover { i: number; px: number; py: number }

type ChartKind = 'line' | 'bar';
type ChartMode = 'daily' | 'total';

/** Classic plot ratio (old fixed 800×220). Height grows with panel width. */
const CHART_ASPECT = 220 / 800;

/**
 * Interactive series chart: line (area) or bars, Daily/Total, hover tooltip.
 *
 * - Default (`absolute` false): `points` are per-day values; Total = running sum
 *   (SeriesExplorer).
 * - `absolute` true (Growth repo chart): `points` are absolute/cumulative totals;
 *   Daily = day-over-day deltas (`daily` prop or derived from `points`); Total = `points`.
 *
 * Full panel width always; height scales with width (not the other way around).
 * viewBox matches measured pixels 1:1 so axis labels stay upright.
 */
export function AreaChart({
  points,
  daily,
  absolute = false,
  percent = false,
  label = '',
}: {
  points: SeriesPoint[];
  /** Optional day-over-day deltas (Growth). Used only when `absolute` is true. */
  daily?: SeriesPoint[];
  /**
   * When true, `points` are absolute totals (stars/downloads snapshots).
   * Daily mode never plots them raw — only deltas.
   */
  absolute?: boolean;
  /**
   * When true, `points` are already percentages (week-over-week growth rate).
   * Axis and tooltip read as signed %, and the Daily/Total switch is hidden —
   * a rate is neither a daily value nor a running total. Everything else
   * (Line/Bar, hover, colors) is identical to the other charts on purpose.
   */
  percent?: boolean;
  label?: string;
  height?: number;
}) {
  const [mode, setMode] = useState<ChartMode>('daily');
  const [kind, setKind] = useState<ChartKind>(() => (absolute || percent ? 'bar' : 'line'));
  const [hover, setHover] = useState<Hover | null>(null);
  const [W, wrapRef] = useElementWidth(800);
  const fmtVal = percent ? fmtRate : fmtNum;

  // Resolve series for the active mode — Daily must never show absolute totals.
  const data = (() => {
    if (percent) return points; // a rate is plotted as given
    if (absolute) {
      if (mode === 'total') return points;
      if (daily && daily.length > 0) return daily;
      return toDeltas(points);
    }
    // points already per-day (site series explorer, etc.)
    return mode === 'total' ? toCumulative(points) : points;
  })();
  // Height follows width — never shrink the plot width to protect labels.
  const H = Math.round(Math.min(520, Math.max(160, W * CHART_ASPECT)));
  const padL = 44, padR = 10, padT = 8, padB = 24;
  const iw = Math.max(1, W - padL - padR);
  const ih = H - padT - padB;
  const vals = data.map((d) => d.count);
  const max = Math.max(1, ...vals);
  const min = Math.min(0, ...vals);
  const span = Math.max(1, max - min);
  const n = data.length;
  const X = (i: number) => padL + (n < 2 ? iw / 2 : (i / (n - 1)) * iw);
  const Y = (v: number) => padT + ih - ((v - min) / span) * ih;
  const y0 = Y(0);
  const slot = n > 0 ? iw / n : iw;
  const barW = Math.max(1, slot * 0.72);
  const barX = (i: number) => padL + i * slot + (slot - barW) / 2;

  const onMove = (e: React.MouseEvent<SVGSVGElement>) => {
    if (!n) return;
    const r = e.currentTarget.getBoundingClientRect();
    // viewBox width === client width → 1 unit = 1 px
    const vbX = ((e.clientX - r.left) / r.width) * W;
    let i: number;
    if (kind === 'bar') {
      i = Math.floor((vbX - padL) / slot);
    } else {
      i = Math.round(((vbX - padL) / iw) * (n - 1));
    }
    i = Math.max(0, Math.min(n - 1, i));
    const px = kind === 'bar' ? barX(i) + barW / 2 : X(i);
    setHover({ i, px, py: Y(data[i].count) });
  };

  const pill = (active: boolean) =>
    `border px-2.5 py-0.5 text-xs h-7 inline-flex items-center ${active ? 'bg-app text-white border-app' : 'border-border text-foreground'}`;

  const tickCount = Math.min(6, Math.max(1, n - 1));
  const tickIdx = Array.from({ length: tickCount + 1 }, (_, t) =>
    Math.round((t / Math.max(1, tickCount)) * (n - 1)),
  );

  return (
    <div className="my-2 border border-border p-3.5" style={{ backgroundColor: 'var(--card)', color: 'var(--card-foreground)' }}>
      <div className="flex justify-end gap-1 mb-1.5 flex-wrap items-center">
        <span className="mr-auto text-[10px] font-mono uppercase tracking-[0.5px] text-muted-foreground">
          {percent
            ? `${label || 'Growth'} %`
            : mode === 'daily'
              ? `Daily ${label || 'delta'}`
              : `Total ${label}`.trim()}
        </span>
        <button type="button" onClick={() => setKind('line')} className={pill(kind === 'line')}>Line</button>
        <button type="button" onClick={() => setKind('bar')} className={pill(kind === 'bar')}>Bar</button>
        {!percent && (
          <>
            <span className="w-2" />
            <button type="button" onClick={() => setMode('daily')} className={pill(mode === 'daily')}>Daily</button>
            <button type="button" onClick={() => setMode('total')} className={pill(mode === 'total')}>Total</button>
          </>
        )}
      </div>
      <div ref={wrapRef} className="relative w-full" style={{ height: H }}>
        {!data.length ? (
          <div className="grid place-items-center text-muted-foreground text-xs" style={{ height: H }}>no data</div>
        ) : (
          <svg
            viewBox={`0 0 ${W} ${H}`}
            preserveAspectRatio="none"
            style={{ width: '100%', height: H, display: 'block' }}
            onMouseMove={onMove}
            onMouseLeave={() => setHover(null)}
          >
            <defs>
              <linearGradient id="ag" x1="0" y1="0" x2="0" y2="1">
                <stop offset="0%" stopColor="var(--color-app)" stopOpacity={0.28} />
                <stop offset="100%" stopColor="var(--color-app)" stopOpacity={0} />
              </linearGradient>
            </defs>
            {[0, 1, 2, 3, 4].map((g) => {
              const raw = min + (span * g) / 4;
              const gv = percent ? raw : Math.round(raw);
              const gy = Y(gv);
              return (
                <g key={g}>
                  <line x1={padL} y1={gy} x2={W - padR} y2={gy} stroke="var(--border)" strokeWidth={1} />
                  <text x={padL - 6} y={gy + 3.5} textAnchor="end" fill="var(--muted-foreground)" fontSize={11}>{fmtVal(gv)}</text>
                </g>
              );
            })}
            {kind === 'line' ? (
              <>
                <polygon
                  points={`${padL},${y0.toFixed(1)} ${data.map((d, i) => `${X(i).toFixed(1)},${Y(d.count).toFixed(1)}`).join(' ')} ${X(n - 1).toFixed(1)},${y0.toFixed(1)}`}
                  fill="url(#ag)"
                />
                <polyline
                  points={data.map((d, i) => `${X(i).toFixed(1)},${Y(d.count).toFixed(1)}`).join(' ')}
                  fill="none" stroke="var(--color-app)" strokeWidth={1.5} strokeLinejoin="round" strokeLinecap="round"
                />
              </>
            ) : (
              data.map((d, i) => {
                const y = Y(d.count);
                const top = Math.min(y, y0);
                const bh = Math.max(1, Math.abs(y0 - y));
                return (
                  <rect
                    key={i}
                    x={barX(i)}
                    y={top}
                    width={barW}
                    height={bh}
                    fill="var(--color-app)"
                    opacity={hover?.i === i ? 1 : 0.88}
                  />
                );
              })
            )}
            {tickIdx.map((i, t) => (
              <text
                key={t}
                x={kind === 'bar' ? barX(i) + barW / 2 : X(i)}
                y={H - 7}
                // Anchor the end labels inward so they cannot spill past the plot
                // (line mode puts the last tick a few px from the right edge).
                textAnchor={t === 0 ? 'start' : t === tickIdx.length - 1 ? 'end' : 'middle'}
                fill="var(--muted-foreground)"
                fontSize={11}
              >
                {fmtBucket(data[i].date)}
              </text>
            ))}
            {hover && kind === 'line' && (
              <>
                <line x1={hover.px} x2={hover.px} y1={padT} y2={padT + ih} stroke="var(--color-app)" strokeWidth={1} />
                <circle cx={hover.px} cy={hover.py} r={2.5} fill="var(--color-app)" />
              </>
            )}
          </svg>
        )}
        {hover && data[hover.i] && (
          <div
            className="absolute pointer-events-none bg-popover border border-border px-2 py-1 text-xs whitespace-nowrap z-10"
            style={{
              left: Math.min(W - 100, Math.max(0, hover.px + 8)),
              top: 4,
            }}
          >
            <b>{fmtBucket(data[hover.i].date)}</b><br />{fmtVal(data[hover.i].count)}
            {percent ? ' WoW' : label ? ' ' + label : ''}
          </div>
        )}
      </div>
    </div>
  );
}

/** Stacked weekly bars for the lifecycle report (new/returning/resurrecting). */
export function StackedBars({ rows }: { rows: LifecycleWeek[] }) {
  if (!rows.length) return <div className="text-muted-foreground text-xs">no data</div>;
  const keys: [keyof LifecycleWeek, string][] = [
    ['new', 'var(--color-app)'], ['returning', 'var(--color-success)'], ['resurrecting', 'var(--color-warning)'],
  ];
  const totals = rows.map((r) => keys.reduce((a, [k]) => a + (Number(r[k]) || 0), 0));
  const max = Math.max(1, ...totals);
  const [W, wrapRef] = useElementWidth(800);
  const H = 200, padT = 8, padB = 22, padL = 8, padR = 8;
  const iw = W - padL - padR, ih = H - padT - padB, gap = iw / rows.length, bw = gap * 0.7;
  return (
    <div ref={wrapRef}>
      <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" style={{ width: '100%', height: H, display: 'block' }}>
        {rows.map((r, i) => {
          let yb = padT + ih;
          const x = padL + i * gap + gap * 0.15;
          return (
            <g key={i}>
              {keys.map(([k, c]) => {
                const v = Number(r[k]) || 0, h = (v / max) * ih;
                yb -= h;
                return <rect key={String(k)} x={x} y={yb} width={bw} height={h} fill={c}><title>{`${r.week} ${String(k)}: ${v}`}</title></rect>;
              })}
              <text x={x + bw / 2} y={H - 6} textAnchor="middle" fill="var(--muted-foreground)" fontSize={11}>{r.week.slice(5)}</text>
            </g>
          );
        })}
      </svg>
      <div className="text-muted-foreground text-xs mt-1 flex gap-4">
        {keys.map(([k, c]) => <span key={String(k)}><span style={{ color: c }}>■</span> {String(k)}</span>)}
      </div>
    </div>
  );
}
