// Admin-key sign-in. The dottie API has no user/password auth — the operator
// pastes DOTTIE_ADMIN_KEY once; it's kept in localStorage and sent as a bearer.
import { useState } from 'react';
import { Card, CardHeader, CardTitle, CardDescription, CardContent } from '@stevederico/skateboard-ui/shadcn/ui/card';
import { Input } from '@stevederico/skateboard-ui/shadcn/ui/input';
import { Button } from '@stevederico/skateboard-ui/shadcn/ui/button';
import { setKey } from '../api';

export function Gate({ onEnter }: { onEnter: () => void }) {
  const [val, setVal] = useState('');
  const submit = () => { const v = val.trim(); if (v) { setKey(v); onEnter(); } };
  return (
    <div className="min-h-dvh grid place-items-center p-4">
      <Card className="w-full max-w-sm">
        <CardHeader>
          <CardTitle className="text-center">Dottie Analytics</CardTitle>
          <CardDescription className="text-center">Paste DOTTIE_ADMIN_KEY to continue.</CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-3">
          <Input
            type="password"
            placeholder="admin key"
            autoComplete="off"
            value={val}
            onChange={(e) => setVal(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') submit(); }}
            className="text-center"
          />
          <Button onClick={submit} className="w-full">Enter</Button>
        </CardContent>
      </Card>
    </div>
  );
}
