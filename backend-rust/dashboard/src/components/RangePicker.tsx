// Shared window picker: All · presets · Custom from/to. Portals into the header
// slot (navbar) when one is mounted, else renders inline. Emits a uniform query
// string every windowed report endpoint understands: `all=1` | `from=&to=` |
// `days=`. Backend `picker_window` resolves all three the same way.
import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useHeaderSlot } from '../lib/headerSlot';

export interface Window {
  params: string;
  label: string;
}

// Presets shown left→right after "All". 24h is a rolling `hours=24` window
// (hourly buckets on series), the rest are calendar-day ranges.
const PRESETS: Window[] = [
  { params: 'hours=24', label: '24h' },
  { params: 'days=7', label: '7d' },
  { params: 'days=30', label: '30d' },
  { params: 'days=90', label: '90d' },
  { params: 'days=180', label: '180d' },
];

export const daysWindow = (days: number): Window => ({
  params: `days=${days}`,
  label: `${days}d`,
});

const field = 'h-8 box-border bg-background text-foreground border border-input px-2.5 text-sm';
const pill = (active: boolean) =>
  `h-8 box-border border px-2.5 text-xs inline-flex items-center ${active ? 'bg-app text-white border-app' : 'border-border text-foreground'}`;

export function RangePicker({ value, onChange }: { value: Window; onChange: (w: Window) => void }) {
  const slot = useHeaderSlot();
  const isAll = value.params === 'all=1';
  const isCustom = value.params.startsWith('from=');

  const [customOpen, setCustomOpen] = useState(false);
  const [draftFrom, setDraftFrom] = useState('');
  const [draftTo, setDraftTo] = useState('');
  const wrap = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!customOpen) return;
    const onDoc = (e: MouseEvent) => {
      if (wrap.current && !wrap.current.contains(e.target as Node)) setCustomOpen(false);
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setCustomOpen(false); };
    document.addEventListener('mousedown', onDoc);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDoc);
      document.removeEventListener('keydown', onKey);
    };
  }, [customOpen]);

  const applyCustom = () => {
    if (!draftFrom || !draftTo) return;
    onChange({ params: `from=${draftFrom}&to=${draftTo}`, label: `${draftFrom}→${draftTo}` });
    setCustomOpen(false);
  };

  const customLabel = isCustom ? value.label : 'Custom';

  const controls = (
    <div className="inline-flex gap-1 h-8 items-center flex-wrap">
      <button type="button" onClick={() => onChange({ params: 'all=1', label: 'All' })} className={pill(isAll)}>
        All
      </button>
      {PRESETS.map((p) => (
        <button key={p.label} type="button" onClick={() => onChange(p)} className={pill(value.params === p.params)}>
          {p.label}
        </button>
      ))}
      <div ref={wrap} className="relative">
        <button
          type="button"
          onClick={() => setCustomOpen((o) => !o)}
          className={pill(isCustom)}
          aria-expanded={customOpen}
          aria-haspopup="dialog"
        >{customLabel}</button>
        {customOpen && (
          <div
            role="dialog"
            aria-label="Custom date range"
            className="absolute right-0 top-full mt-1.5 z-50 border border-border p-3 shadow-md min-w-[220px]"
            style={{ backgroundColor: 'var(--card)', color: 'var(--card-foreground)' }}
          >
            <div className="flex flex-col gap-2">
              <label className="flex flex-col gap-1 text-[11px] uppercase tracking-[0.5px] text-muted-foreground">
                From
                <input type="date" value={draftFrom} onChange={(e) => setDraftFrom(e.target.value)} className={`${field} w-full`} />
              </label>
              <label className="flex flex-col gap-1 text-[11px] uppercase tracking-[0.5px] text-muted-foreground">
                To
                <input type="date" value={draftTo} onChange={(e) => setDraftTo(e.target.value)} className={`${field} w-full`} />
              </label>
              <button
                type="button"
                onClick={applyCustom}
                disabled={!draftFrom || !draftTo}
                className={`h-8 border text-xs ${draftFrom && draftTo ? 'bg-app text-white border-app' : 'border-border text-muted-foreground opacity-50'}`}
              >Apply</button>
            </div>
          </div>
        )}
      </div>
    </div>
  );

  return slot ? createPortal(controls, slot) : <div className="mb-2.5">{controls}</div>;
}
