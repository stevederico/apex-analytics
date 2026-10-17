// Tiny data-loading helper so every view follows the same loading/error/data
// shape. React error boundaries don't catch async effect errors, so each view
// owns its fetch state via useAsync + <Async>. useStream keeps the connection
// open (gRPC-Web server streaming) and replaces data on each chunk.
import { useEffect, useState } from 'react';
import type { ReactNode } from 'react';
import { Loading, ErrorBox } from '../components/ui';
import { AuthError, getKey } from '../api';
import { GrpcError, grpcJsonStream } from '../grpc';

export interface AsyncState<T> { loading: boolean; error: string | null; data: T | null }

export function useAsync<T>(
  fn: () => Promise<T>,
  deps: unknown[],
  opts?: { keepPrevious?: boolean },
): AsyncState<T> {
  const keep = opts?.keepPrevious === true;
  const [state, setState] = useState<AsyncState<T>>({ loading: true, error: null, data: null });
  useEffect(() => {
    let live = true;
    setState((s) => (
      keep && s.data != null
        ? { loading: true, error: null, data: s.data }
        : { loading: true, error: null, data: null }
    ));
    fn().then(
      (data) => { if (live) setState({ loading: false, error: null, data }); },
      (e: unknown) => {
        if (live) {
          setState((s) => ({
            loading: false,
            error: e instanceof Error ? e.message : String(e),
            data: keep ? s.data : null,
          }));
        }
      },
    );
    return () => { live = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return state;
}

/**
 * Live gRPC-Web server stream. First chunk fills data; later chunks update in place.
 * Abort on unmount / dep change. Pass `method: null` to disable (hooks stay stable).
 */
export function useStream<T>(
  method: string | null,
  encodeReq: () => Uint8Array,
  deps: unknown[],
): AsyncState<T> {
  const [state, setState] = useState<AsyncState<T>>({ loading: true, error: null, data: null });
  useEffect(() => {
    if (!method) {
      setState({ loading: false, error: null, data: null });
      return;
    }
    const ac = new AbortController();
    setState((s) => (
      s.data != null
        ? { loading: false, error: null, data: s.data }
        : { loading: true, error: null, data: null }
    ));
    (async () => {
      try {
        for await (const item of grpcJsonStream<T>(method, encodeReq(), getKey(), ac.signal)) {
          if (ac.signal.aborted) return;
          setState({ loading: false, error: null, data: item });
        }
      } catch (e: unknown) {
        if (ac.signal.aborted) return;
        if (e instanceof GrpcError && (e.code === 5 || e.code === 16)) {
          setState((s) => ({ loading: false, error: new AuthError().message, data: s.data }));
          return;
        }
        setState((s) => ({
          loading: false,
          error: e instanceof Error ? e.message : String(e),
          data: s.data,
        }));
      }
    })();
    return () => ac.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return state;
}

export function Async<T>({
  state, children, keepPrevious,
}: {
  state: AsyncState<T>;
  children: (data: T) => ReactNode;
  /** When true, keep rendering stale data while a refetch is in flight. */
  keepPrevious?: boolean;
}) {
  if (state.loading && !(keepPrevious && state.data != null)) return <Loading />;
  if (state.error && state.data == null) return <ErrorBox message={state.error} />;
  if (state.data == null) return null;
  return <>{children(state.data)}</>;
}
