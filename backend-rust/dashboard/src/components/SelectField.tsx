// Custom select (skateboard Select) — never native <select>.
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@stevederico/skateboard-ui/shadcn/ui/select';

export interface SelectOption {
  value: string;
  label: string;
}

interface Props {
  value: string;
  onChange: (value: string) => void;
  options: SelectOption[];
  /** Shown when value is empty / missing from options. */
  placeholder?: string;
  /** sm matches SeriesExplorer header height. */
  size?: 'sm' | 'default';
  className?: string;
  'aria-label'?: string;
}

export function SelectField({
  value,
  onChange,
  options,
  placeholder = 'Select…',
  size = 'sm',
  className,
  'aria-label': ariaLabel,
}: Props) {
  // Radix-style Select rejects empty string values — map to a sentinel when needed.
  const empty = options.some((o) => o.value === '');
  const internal = value === '' && empty ? '__empty__' : value;
  const items = options.map((o) =>
    o.value === '' ? { value: '__empty__', label: o.label } : o,
  );

  return (
    <Select
      value={internal || undefined}
      onValueChange={(v) => onChange(v === '__empty__' ? '' : v)}
    >
      <SelectTrigger
        size={size}
        className={className}
        aria-label={ariaLabel}
      >
        <SelectValue placeholder={placeholder} />
      </SelectTrigger>
      <SelectContent>
        {items.map((o) => (
          <SelectItem key={o.value} value={o.value}>
            {o.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
