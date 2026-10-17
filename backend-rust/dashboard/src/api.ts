import {
  emptyMsg,
  encodeBreakdown,
  encodeHours,
  encodeIssues,
  encodePatchIssue,
  encodeSiteEvents,
  encodeUsers,
  encodeWeeks,
  GrpcError,
  grpcJson,
} from './grpc';

export const KEY = 'dottie_admin_key';

export const getKey = (): string => localStorage.getItem(KEY) || '';
export const setKey = (v: string): void => localStorage.setItem(KEY, v);
export const clearKey = (): void => localStorage.removeItem(KEY);

/** Thrown when the admin key is missing or wrong (server NOT_FOUND / 404). */
export class AuthError extends Error {
  constructor() { super('auth: check admin key'); this.name = 'AuthError'; }
}

/**
 * Admin API: gRPC-Web for dashboard GETs where mapped; REST fallback for growth
 * (downloads/metrics/repos), snapshots, and other unmapped routes.
 */
export async function api<T>(path: string, opts?: RequestInit): Promise<T> {
  const token = getKey();
  try {
    return await dispatch<T>(path, opts, token);
  } catch (e) {
    if (e instanceof GrpcError && (e.code === 5 || e.code === 16)) throw new AuthError();
    if (e instanceof AuthError) throw e;
    throw e;
  }
}

/** Authed binary GET (e.g. a feedback screenshot) → object URL for an <img>.
 *  An <img src> can't carry the bearer, so fetch it and hand back a blob URL;
 *  the caller revokes it. */
export async function apiBlobUrl(path: string): Promise<string> {
  const res = await fetch(path, { headers: { authorization: 'Bearer ' + getKey() } });
  if (res.status === 404 || res.status === 401) throw new AuthError();
  if (!res.ok) throw new Error(path + ' → ' + res.status);
  return URL.createObjectURL(await res.blob());
}

function q(path: string): URLSearchParams {
  const i = path.indexOf('?');
  return new URLSearchParams(i >= 0 ? path.slice(i + 1) : '');
}

function base(path: string): string {
  const i = path.indexOf('?');
  return i >= 0 ? path.slice(0, i) : path;
}

/** Plain REST fetch with bearer — growth + any non-gRPC path. */
async function restJson<T>(path: string, opts: RequestInit | undefined, token: string): Promise<T> {
  const res = await fetch(path, {
    ...opts,
    headers: {
      authorization: 'Bearer ' + token,
      'content-type': 'application/json',
      ...(opts?.headers),
    },
  });
  if (res.status === 404 || res.status === 401) throw new AuthError();
  if (!res.ok) {
    let detail = path + ' → ' + res.status;
    try {
      const j = await res.json() as { error?: string; message?: string };
      if (j?.message) detail = j.message;
      else if (j?.error) detail = j.error;
    } catch { /* ignore */ }
    throw new Error(detail);
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  if (!text) return undefined as T;
  return JSON.parse(text) as T;
}

async function dispatch<T>(path: string, opts: RequestInit | undefined, token: string): Promise<T> {
  const method = (opts?.method ?? 'GET').toUpperCase();
  const p = base(path);
  const params = q(path);

  // PATCH/POST /api/issues/:id → gRPC
  const issueMatch = /^\/api\/issues\/(\d+)$/.exec(p);
  if (issueMatch && (method === 'PATCH' || method === 'POST')) {
    const body = opts?.body ? JSON.parse(String(opts.body)) as { status?: string } : {};
    const id = Number(issueMatch[1]);
    return grpcJson('PatchIssue', encodePatchIssue(id, body.status ?? ''), token);
  }

  // Growth + import + any non-GET without a gRPC twin → REST.
  if (
    method !== 'GET'
    || p.startsWith('/api/downloads')
    || p.startsWith('/api/metrics')
    || p.startsWith('/api/repos')
    || p.startsWith('/api/import/')
  ) {
    return restJson<T>(path, opts, token);
  }

  switch (p) {
    case '/api/stats':
      return grpcJson('GetStats', emptyMsg(), token);
    // series / retention / activation / funnel / attribution → REST (default
    // case): the gRPC messages can't carry the picker window (hours/from/to/all),
    // and REST resolves days/hours/from/to/all uniformly (series also serves
    // hourly buckets for the 24h view). gRPC twins remain for scripts/back-compat.
    case '/api/stats/lifecycle':
      return grpcJson('GetLifecycle', encodeWeeks(params.get('weeks') ?? undefined), token);
    case '/api/stats/breakdown':
      return grpcJson('GetBreakdown', encodeBreakdown(params.get('by') ?? undefined), token);
    case '/api/stats/users':
      return grpcJson('GetUsers', encodeUsers(params.get('limit') ?? undefined), token);
    case '/api/stats/issues':
      return grpcJson('GetIssues', encodeIssues(params.get('status') ?? undefined), token);
    case '/api/stats/landing-events':
      return grpcJson('GetLandingEvents', encodeHours(params.get('hours') ?? undefined), token);
    case '/api/stats/landing-funnel':
      return grpcJson('GetLandingFunnel', encodeHours(params.get('hours') ?? undefined), token);
    case '/api/sites':
      return grpcJson('ListSites', emptyMsg(), token);
    case '/api/site/events':
      return grpcJson(
        'GetSiteEvents',
        encodeSiteEvents(params.get('site') ?? '', params.get('hours') ?? undefined),
        token,
      );
    // site summary/series/pages/referrers/breakdown → REST (default case): the
    // picker window (from/to/hours/all) needs uniform resolution the gRPC
    // messages can't carry, and REST de-streams them (Cloudflare-safe). gRPC
    // twins remain for back-compat.
    default:
      // Unmapped GET → REST (future endpoints).
      return restJson<T>(path, opts, token);
  }
}
