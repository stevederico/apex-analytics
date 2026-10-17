import { useState } from 'react';
import { api } from '../api';
import type { UsersResponse } from '../types';
import { useAsync, Async } from '../lib/async';
import { Tiles, Section, DataTable } from '../components/ui';
import { fmtTs } from '../lib/format';

export default function Users() {
  const [search, setSearch] = useState('');
  const s = useAsync(() => api<UsersResponse>('/api/stats/users?limit=1000'), []);
  return (
    <Async state={s}>
      {(d) => {
        const q = search.trim().toLowerCase();
        const rows = q
          ? d.rows.filter((r) => [r.name, r.email, r.version].some((v) => String(v ?? '').toLowerCase().includes(q)))
          : d.rows;
        return (
          <>
            <Tiles items={[['Total users', d.total]]} />

            <label className="text-sm text-muted-foreground flex items-center gap-2 mb-2">
              Search
              <input
                type="text"
                placeholder="name / email / version"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                className="bg-background text-foreground border border-input px-2 py-1 text-sm"
                style={{ width: 280 }}
              />
            </label>

            <Section title={`${rows.length} of ${d.total}`}>
              <DataTable
                cols={[
                  { label: 'Name', key: 'name' },
                  { label: 'Email', key: 'email' },
                  { label: 'Ver', key: 'version' },
                  { label: 'Country', key: 'country' },
                  { label: 'Signup', render: (r) => <span className="text-muted-foreground">{fmtTs(r.signed_up_at)}</span>, sortValue: (r) => r.signed_up_at },
                  { label: 'Last seen', render: (r) => <span className="text-muted-foreground">{fmtTs(r.last_seen)}</span>, sortValue: (r) => r.last_seen },
                  { label: 'Active days', key: 'active_days', num: true },
                  { label: 'Msgs', key: 'msgs', num: true },
                  { label: 'Voice', key: 'voice', num: true },
                ]}
                rows={rows}
              />
            </Section>
          </>
        );
      }}
    </Async>
  );
}
