// Formatting helpers shared across views. React escapes text nodes, so there is
// no HTML-escaping helper here (unlike the legacy vanilla page).

export const fmtNum = (n: number | null | undefined): string => (n ?? 0).toLocaleString();

export const fmtTs = (v: string | number | null | undefined): string => {
  if (v == null) return '—';
  const n = typeof v === 'number' ? v : Date.parse(v);
  return Number.isFinite(n) ? new Date(n).toLocaleString() : String(v);
};

/** Humanize a millisecond duration to the coarsest sensible unit. */
export const dur = (ms: number | null | undefined): string => {
  if (ms == null) return '—';
  const s = Math.round(ms / 1000);
  if (s < 60) return s + 's';
  const m = Math.round(s / 60);
  if (m < 60) return m + 'm';
  const h = Math.round(m / 60);
  if (h < 48) return h + 'h';
  return Math.round(h / 24) + 'd';
};

/** Parse a YYYY-MM-DD as local midnight (avoids the UTC previous-day shift). */
export const localDate = (s: string): Date => new Date(s + 'T00:00:00');

export const fmtDay = (s: string): string =>
  new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' }).format(localDate(s));

/** Series x-axis label: hour for ISO-datetime points (hourly 24h view), else day.
 *  Hourly points carry a full ISO-Z timestamp (contains 'T'); daily are YYYY-MM-DD. */
export const fmtBucket = (s: string): string =>
  s.includes('T')
    ? new Intl.DateTimeFormat(undefined, { hour: 'numeric' }).format(new Date(s))
    : fmtDay(s);

/** rate ∈ [0,1] → "NN%", or "—" when null. */
export const pct = (rate: number | null | undefined): string =>
  rate == null ? '—' : Math.round(rate * 100) + '%';

/** Signed percentage point → "+2.4%" / "-1.0%" (growth rates, already in %). */
export const fmtRate = (v: number | null | undefined): string =>
  v == null ? '—' : `${v >= 0 ? '+' : ''}${v.toFixed(1)}%`;

/** Sum of daily series counts. */
export const seriesSum = (pts: { count: number }[]): number =>
  pts.reduce((a, p) => a + p.count, 0);

/** Integer mean of daily series counts (0 when empty). */
export const seriesAvg = (pts: { count: number }[]): number =>
  pts.length ? Math.round(seriesSum(pts) / pts.length) : 0;

/** Peak daily count in a series. */
export const seriesMax = (pts: { count: number }[]): number =>
  pts.length ? Math.max(...pts.map((p) => p.count)) : 0;
