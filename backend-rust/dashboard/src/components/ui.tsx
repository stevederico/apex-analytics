// Shared presentational helpers used by every view: metric tiles, stat cards
// (with optional WoW delta), a generic column-driven table, section headings,
// and load/error states. Styled with shadcn tokens + the brutalist theme.
import { useMemo, useState } from 'react';
import type { ReactNode } from 'react';
import {
  Table, TableHeader, TableBody, TableRow, TableHead, TableCell,
} from '@stevederico/skateboard-ui/shadcn/ui/table';

/** Hard-edged content container — lifts graphs/tables/KPIs off the page bg.
 *  Uses var(--card) inline so the surface always matches Panel/tables. */
const surface = 'border border-border p-3.5';
const surfaceStyle = { backgroundColor: 'var(--card)', color: 'var(--card-foreground)' } as const;

export function Panel({ children, className = '' }: { children: ReactNode; className?: string }) {
  return (
    <div className={`${surface} ${className}`} style={surfaceStyle}>{children}</div>
  );
}

export function Section({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="mt-6">
      <h2 className="text-[12px] tracking-[1px] text-muted-foreground mb-2">{title}</h2>
      {children != null ? <Panel>{children}</Panel> : null}
    </div>
  );
}

export function Loading() {
  return <p className="text-muted-foreground text-sm">Loading…</p>;
}

export function ErrorBox({ message }: { message: string }) {
  return <div className="text-bad border border-bad bg-bad/10 p-3.5 text-sm">{message}</div>;
}

/** Compact KPI tiles. Full-width strip, same surface as table panels. */
export function Tiles({ items }: { items: [string, ReactNode, ReactNode?][] }) {
  return (
    <div
      className="grid w-full mb-4 border border-border divide-x divide-border"
      style={{
        ...surfaceStyle,
        gridTemplateColumns: `repeat(${Math.max(1, items.length)}, minmax(0, 1fr))`,
      }}
    >
      {items.map(([k, v, sub], i) => (
        <div key={i} className="p-3.5 min-w-0">
          <div className="text-muted-foreground uppercase text-[11px] tracking-[0.5px]">{k}</div>
          <div className="text-[26px] mt-1 leading-tight">{v}{sub != null && <small className="text-xs text-muted-foreground"> {sub}</small>}</div>
        </div>
      ))}
    </div>
  );
}

export interface StatCard {
  label: string;
  value: ReactNode;
  sub?: ReactNode;
  /** Signed % change vs prior period (or absolute when `deltaAbs`). */
  delta?: number | null;
  /** When true, treat `delta` as absolute (not percent) — still shows +/−. */
  deltaAbs?: boolean;
  /** Invert green/red (for bounce rate: down is good). */
  reverseDelta?: boolean;
  /** Clickable KPI (e.g. switch chart metric). */
  onClick?: () => void;
  active?: boolean;
}

/** KPI cards: full-width strip, one shared panel surface (matches table Panels). */
export function StatCards({ cards }: { cards: StatCard[] }) {
  return (
    <div
      className="grid w-full mb-4 border border-border divide-x divide-border"
      style={{
        ...surfaceStyle,
        gridTemplateColumns: `repeat(${Math.max(1, cards.length)}, minmax(0, 1fr))`,
      }}
    >
      {cards.map((c, i) => {
        const good = c.delta != null && (c.reverseDelta ? c.delta <= 0 : c.delta >= 0);
        const clickable = c.onClick != null;
        const Tag = clickable ? 'button' : 'div';
        return (
          <Tag
            key={i}
            type={clickable ? 'button' : undefined}
            onClick={c.onClick}
            className={`p-3.5 min-w-0 text-left ${clickable ? 'cursor-pointer hover:bg-secondary/40 transition-colors' : ''} ${c.active ? 'bg-secondary/60' : ''}`}
          >
            <div className={`uppercase text-[11px] tracking-[0.5px] ${c.active ? 'text-app' : 'text-muted-foreground'}`}>{c.label}</div>
            <div className="text-[26px] mt-1 leading-tight">
              {c.value}
              {c.sub != null && <small className="text-xs text-muted-foreground"> {c.sub}</small>}
              {c.delta != null && (
                <span className={`text-xs ml-1.5 ${good ? 'text-success' : 'text-bad'}`}>
                  {c.delta >= 0 ? '+' : ''}
                  {c.deltaAbs ? c.delta.toFixed(c.delta % 1 === 0 ? 0 : 1) : c.delta.toFixed(1) + '%'}
                </span>
              )}
            </div>
          </Tag>
        );
      })}
    </div>
  );
}

/** Simple tab strip for table modes (path / url / entry / exit, etc.). */
export function TabStrip({
  tabs, value, onChange,
}: {
  tabs: [string, string][];
  value: string;
  onChange: (v: string) => void;
}) {
  return (
    <div className="inline-flex gap-1 mb-2.5 flex-wrap">
      {tabs.map(([id, label]) => (
        <button
          key={id}
          type="button"
          onClick={() => onChange(id)}
          className={`h-8 box-border border px-2.5 text-xs inline-flex items-center ${value === id ? 'bg-app text-white border-app' : 'border-border text-foreground'}`}
        >{label}</button>
      ))}
    </div>
  );
}

export interface Column<T> {
  label: string;
  num?: boolean;
  key?: keyof T;
  render?: (row: T) => ReactNode;
  /** Value used for click-to-sort. Defaults to `row[key]`. Give this for
   *  render-only columns (e.g. a timestamp shown via a formatter). Set to
   *  `null`/omit with no `key` to make the column unsortable. */
  sortValue?: (row: T) => string | number | null | undefined;
}

type Dir = 'asc' | 'desc';

const sortableKey = <T,>(c: Column<T>): boolean => c.sortValue != null || c.key != null;

const cellValue = <T,>(c: Column<T>, row: T): string | number | null | undefined =>
  c.sortValue != null ? c.sortValue(row) : c.key != null ? (row[c.key] as unknown as string | number) : null;

/** null/undefined last; numbers numerically; else case-insensitive string. */
function compare(a: string | number | null | undefined, b: string | number | null | undefined): number {
  const an = a == null, bn = b == null;
  if (an || bn) return an === bn ? 0 : an ? 1 : -1;
  if (typeof a === 'number' && typeof b === 'number') return a - b;
  const na = Number(a), nb = Number(b);
  if (Number.isFinite(na) && Number.isFinite(nb) && String(a).trim() !== '' && String(b).trim() !== '') return na - nb;
  return String(a).localeCompare(String(b), undefined, { numeric: true });
}

/** Generic table driven by a column spec. `num` right-aligns. Click a sortable
 *  header to sort (toggles asc/desc); default order is the rows as passed.
 *  Borderless shell — parent Section/Panel supplies the container. */
export function DataTable<T>({ cols, rows }: { cols: Column<T>[]; rows: T[] }) {
  const [sort, setSort] = useState<{ ci: number; dir: Dir } | null>(null);

  const sorted = useMemo(() => {
    if (!sort) return rows;
    const col = cols[sort.ci];
    if (!col || !sortableKey(col)) return rows;
    const mul = sort.dir === 'asc' ? 1 : -1;
    // Stable sort on a copy; never mutate the incoming rows.
    return [...rows].sort((a, b) => mul * compare(cellValue(col, a), cellValue(col, b)));
  }, [rows, cols, sort]);

  const toggle = (ci: number) =>
    setSort((s) => (s && s.ci === ci ? { ci, dir: s.dir === 'asc' ? 'desc' : 'asc' } : { ci, dir: 'asc' }));

  return (
    <Table>
      <TableHeader>
        <TableRow className="border-border hover:bg-transparent">
          {cols.map((c, i) => {
            const on = sortableKey(c);
            const active = sort?.ci === i;
            return (
              <TableHead
                key={i}
                aria-sort={active ? (sort.dir === 'asc' ? 'ascending' : 'descending') : undefined}
                className={`text-muted-foreground uppercase text-[11px] tracking-[0.5px] ${c.num ? 'text-right' : ''} ${on ? 'cursor-pointer select-none hover:text-foreground' : ''}`}
                onClick={on ? () => toggle(i) : undefined}
              >
                {c.label}
                {active && <span className="ml-1 text-app">{sort.dir === 'asc' ? '▲' : '▼'}</span>}
              </TableHead>
            );
          })}
        </TableRow>
      </TableHeader>
      <TableBody>
        {sorted.map((r, ri) => (
          <TableRow key={ri} className="border-border">
            {cols.map((c, ci) => (
              <TableCell key={ci} className={`whitespace-nowrap ${c.num ? 'text-right' : ''}`}>
                {c.render ? c.render(r) : c.key != null ? String(r[c.key] ?? '') : ''}
              </TableCell>
            ))}
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

/** Status/label pill, colored by kind. */
export function Pill({ kind, children }: { kind?: 'active' | 'regressed' | 'resolved'; children: ReactNode }) {
  const color =
    kind === 'active' ? 'text-warning border-warning'
    : kind === 'regressed' ? 'text-bad border-bad'
    : kind === 'resolved' ? 'text-success border-success'
    : 'text-muted-foreground border-border';
  return <span className={`px-1.5 py-px border uppercase text-[11px] ${color}`}>{children}</span>;
}

/** Horizontal conversion bar (funnel steps). frac ∈ [0,1]. */
export function ConvBar({ frac }: { frac: number }) {
  return (
    <div className="bg-border h-2.5 w-[140px]">
      <div className="bg-app h-2.5" style={{ width: `${(frac * 100).toFixed(0)}%` }} />
    </div>
  );
}
