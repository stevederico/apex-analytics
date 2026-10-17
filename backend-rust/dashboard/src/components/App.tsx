import { useEffect, useState } from 'react';
import { getKey } from '../api';
import { Gate } from './Gate';
import { Shell } from './Shell';

export default function App() {
  const [keyed, setKeyed] = useState(() => !!getKey());
  const [current, setCurrent] = useState(() => location.hash.slice(1) || 'overview');

  // Keep the active tab in sync with back/forward navigation.
  useEffect(() => {
    const onHash = () => { const h = location.hash.slice(1); if (h) setCurrent(h); };
    window.addEventListener('hashchange', onHash);
    return () => window.removeEventListener('hashchange', onHash);
  }, []);

  const navigate = (id: string) => { setCurrent(id); location.hash = id; };

  if (!keyed) return <Gate onEnter={() => setKeyed(true)} />;
  return <Shell current={current} navigate={navigate} onSignOut={() => setKeyed(false)} />;
}
