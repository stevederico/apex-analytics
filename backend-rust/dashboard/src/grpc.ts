/**
 * Minimal gRPC-Web client for dottie.admin.v1.Admin.
 *
 * Unary + server streaming (xAI-style). Hand-rolled wire codec (no codegen).
 * Framing: gRPC-Web binary (`application/grpc-web+proto`).
 */

const SERVICE = 'dottie.admin.v1.Admin';

// ---- protobuf wire (subset) ------------------------------------------------

function encodeVarint(n: number): number[] {
  const out: number[] = [];
  let v = n >>> 0;
  while (v > 0x7f) {
    out.push((v & 0x7f) | 0x80);
    v >>>= 7;
  }
  out.push(v);
  return out;
}

function encodeKey(field: number, wire: number): number[] {
  return encodeVarint((field << 3) | wire);
}

function encodeString(field: number, s: string): Uint8Array {
  if (!s) return new Uint8Array(0);
  const bytes = new TextEncoder().encode(s);
  const head = [...encodeKey(field, 2), ...encodeVarint(bytes.length)];
  const out = new Uint8Array(head.length + bytes.length);
  out.set(head, 0);
  out.set(bytes, head.length);
  return out;
}

function encodeInt64(field: number, n: number): Uint8Array {
  if (n === 0) return new Uint8Array(0);
  const head = encodeKey(field, 0);
  const body = encodeVarint(n);
  const out = new Uint8Array(head.length + body.length);
  out.set(head, 0);
  out.set(body, head.length);
  return out;
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const total = parts.reduce((n, p) => n + p.length, 0);
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

function decodeVarint(buf: Uint8Array, i: number): [number, number] {
  let result = 0;
  let shift = 0;
  while (i < buf.length) {
    const b = buf[i++]!;
    result |= (b & 0x7f) << shift;
    if ((b & 0x80) === 0) return [result >>> 0, i];
    shift += 7;
  }
  throw new Error('truncated varint');
}

/** Decode field 1 string from a JsonBody (or empty → ""). */
export function decodeJsonBody(buf: Uint8Array): string {
  let i = 0;
  while (i < buf.length) {
    const [key, ni] = decodeVarint(buf, i);
    i = ni;
    const field = key >>> 3;
    const wire = key & 7;
    if (wire === 2) {
      const [len, nj] = decodeVarint(buf, i);
      i = nj;
      const slice = buf.subarray(i, i + len);
      i += len;
      if (field === 1) return new TextDecoder().decode(slice);
    } else if (wire === 0) {
      [, i] = decodeVarint(buf, i);
    } else {
      throw new Error('unsupported wire type ' + wire);
    }
  }
  return '';
}

// ---- message encoders ------------------------------------------------------

export const emptyMsg = (): Uint8Array => new Uint8Array(0);

export function encodeSeries(m: {
  metric: string; days?: string; from?: string; to?: string;
}): Uint8Array {
  return concat(
    encodeString(1, m.metric),
    encodeString(2, m.days ?? ''),
    encodeString(3, m.from ?? ''),
    encodeString(4, m.to ?? ''),
  );
}

export function encodeWeeks(weeks?: string): Uint8Array {
  return encodeString(1, weeks ?? '');
}

export function encodeHours(hours?: string): Uint8Array {
  return encodeString(1, hours ?? '');
}

export function encodeBreakdown(by?: string): Uint8Array {
  return encodeString(1, by ?? '');
}

export function encodeUsers(limit?: string): Uint8Array {
  return encodeString(1, limit ?? '');
}

export function encodeIssues(status?: string): Uint8Array {
  return encodeString(1, status ?? '');
}

export function encodePatchIssue(id: number, status: string): Uint8Array {
  return concat(encodeInt64(1, id), encodeString(2, status));
}

export function encodeSiteRange(m: {
  site: string; days?: string; from?: string; to?: string; hours?: string;
}): Uint8Array {
  return concat(
    encodeString(1, m.site),
    encodeString(2, m.days ?? ''),
    encodeString(3, m.from ?? ''),
    encodeString(4, m.to ?? ''),
    encodeString(5, m.hours ?? ''),
  );
}

export function encodeSiteSeries(m: {
  site: string; metric: string; days?: string; from?: string; to?: string;
}): Uint8Array {
  return concat(
    encodeString(1, m.site),
    encodeString(2, m.metric),
    encodeString(3, m.days ?? ''),
    encodeString(4, m.from ?? ''),
    encodeString(5, m.to ?? ''),
  );
}

export function encodeSiteEvents(site: string, hours?: string): Uint8Array {
  return concat(encodeString(1, site), encodeString(2, hours ?? ''));
}

export function encodeSitePages(m: {
  site: string; kind?: string; days?: string; from?: string; to?: string; hours?: string;
}): Uint8Array {
  return concat(
    encodeString(1, m.site),
    encodeString(2, m.kind ?? ''),
    encodeString(3, m.days ?? ''),
    encodeString(4, m.from ?? ''),
    encodeString(5, m.to ?? ''),
    encodeString(6, m.hours ?? ''),
  );
}

export function encodeSiteBreakdown(m: {
  site: string; by?: string; days?: string; from?: string; to?: string; hours?: string;
}): Uint8Array {
  return concat(
    encodeString(1, m.site),
    encodeString(2, m.by ?? ''),
    encodeString(3, m.days ?? ''),
    encodeString(4, m.from ?? ''),
    encodeString(5, m.to ?? ''),
    encodeString(6, m.hours ?? ''),
  );
}

function encodeU32(field: number, n: number): Uint8Array {
  if (!n) return new Uint8Array(0);
  return encodeInt64(field, n);
}

/** StreamOpts { interval_ms } */
export function encodeStreamOpts(intervalMs = 3000): Uint8Array {
  return encodeU32(1, intervalMs);
}

export function encodeSeriesStream(m: {
  metric: string; days?: string; from?: string; to?: string; intervalMs?: number;
}): Uint8Array {
  return concat(
    encodeString(1, m.metric),
    encodeString(2, m.days ?? ''),
    encodeString(3, m.from ?? ''),
    encodeString(4, m.to ?? ''),
    encodeU32(5, m.intervalMs ?? 3000),
  );
}

export function encodeIssuesStream(status?: string, intervalMs = 3000): Uint8Array {
  return concat(encodeString(1, status ?? ''), encodeU32(2, intervalMs));
}

export function encodeSiteSummaryStream(m: {
  site: string; days?: string; from?: string; to?: string; hours?: string; intervalMs?: number;
}): Uint8Array {
  return concat(
    encodeString(1, m.site),
    encodeString(2, m.days ?? ''),
    encodeString(3, m.from ?? ''),
    encodeString(4, m.to ?? ''),
    encodeString(5, m.hours ?? ''),
    encodeU32(6, m.intervalMs ?? 3000),
  );
}

export function encodeSiteSeriesStream(m: {
  site: string; metric: string; days?: string; from?: string; to?: string; intervalMs?: number;
}): Uint8Array {
  return concat(
    encodeString(1, m.site),
    encodeString(2, m.metric),
    encodeString(3, m.days ?? ''),
    encodeString(4, m.from ?? ''),
    encodeString(5, m.to ?? ''),
    encodeU32(6, m.intervalMs ?? 3000),
  );
}

// ---- gRPC-Web transport ----------------------------------------------------

function frameRequest(msg: Uint8Array): Uint8Array {
  const out = new Uint8Array(5 + msg.length);
  out[0] = 0; // data, uncompressed
  new DataView(out.buffer).setUint32(1, msg.length, false);
  out.set(msg, 5);
  return out;
}

/** Parse gRPC-Web response frames; return first data payload + grpc-status. */
function parseResponse(buf: Uint8Array): { data: Uint8Array; status: number; message: string } {
  let i = 0;
  let data = new Uint8Array(0);
  let status = 0;
  let message = '';
  while (i + 5 <= buf.length) {
    const flags = buf[i]!;
    const len = new DataView(buf.buffer, buf.byteOffset + i + 1, 4).getUint32(0, false);
    i += 5;
    if (i + len > buf.length) break;
    const payload = buf.subarray(i, i + len);
    i += len;
    if ((flags & 0x80) !== 0) {
      // trailers: "grpc-status: 0\r\ngrpc-message: ...\r\n"
      const text = new TextDecoder().decode(payload);
      for (const line of text.split(/\r?\n/)) {
        const c = line.indexOf(':');
        if (c < 0) continue;
        const k = line.slice(0, c).trim().toLowerCase();
        const v = line.slice(c + 1).trim();
        if (k === 'grpc-status') status = parseInt(v, 10) || 0;
        if (k === 'grpc-message') message = decodeURIComponent(v.replace(/\+/g, ' '));
      }
    } else {
      // Copy into a fresh ArrayBuffer-backed Uint8Array (TS 5.x buffer typing).
      data = new Uint8Array(payload);
    }
  }
  return { data, status, message };
}

export class GrpcError extends Error {
  constructor(
    public readonly code: number,
    message: string,
  ) {
    super(message || 'grpc status ' + code);
    this.name = 'GrpcError';
  }
}

/** Unary gRPC-Web call → raw protobuf response bytes. */
export async function grpcUnary(
  method: string,
  req: Uint8Array,
  token: string,
): Promise<Uint8Array> {
  const framed = frameRequest(req);
  const res = await fetch('/' + SERVICE + '/' + method, {
    method: 'POST',
    headers: {
      'content-type': 'application/grpc-web+proto',
      'x-grpc-web': '1',
      'x-user-agent': 'dottie-dashboard',
      authorization: 'Bearer ' + token,
    },
    body: framed.buffer.slice(framed.byteOffset, framed.byteOffset + framed.byteLength) as ArrayBuffer,
  });
  if (res.status === 404 || res.status === 401) {
    throw new GrpcError(5, 'not_found');
  }
  if (!res.ok) {
    throw new Error(method + ' → HTTP ' + res.status);
  }
  // Errors often put grpc-status on HTTP headers (empty body); success uses
  // trailers in the gRPC-Web body frame.
  const hdrStatus = res.headers.get('grpc-status');
  const hdrMsg = res.headers.get('grpc-message') ?? '';
  if (hdrStatus != null && hdrStatus !== '0') {
    throw new GrpcError(parseInt(hdrStatus, 10) || 2, hdrMsg || 'not_found');
  }
  const buf = new Uint8Array(await res.arrayBuffer());
  const { data, status, message } = parseResponse(buf);
  if (status !== 0) throw new GrpcError(status, message);
  return data;
}

/** Unary call that returns JsonBody.json parsed as T. */
export async function grpcJson<T>(
  method: string,
  req: Uint8Array,
  token: string,
): Promise<T> {
  const data = await grpcUnary(method, req, token);
  const json = decodeJsonBody(data);
  // yagni: same trust model as the old REST client — our own report JSON.
  return JSON.parse(json || 'null') as T;
}

function appendBuf(a: Uint8Array, b: Uint8Array): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

/**
 * Server-streaming gRPC-Web → async generator of parsed JsonBody values.
 * Cancel via AbortSignal (navigate away / remount).
 */
export async function* grpcJsonStream<T>(
  method: string,
  req: Uint8Array,
  token: string,
  signal?: AbortSignal,
): AsyncGenerator<T> {
  const framed = frameRequest(req);
  const res = await fetch('/' + SERVICE + '/' + method, {
    method: 'POST',
    headers: {
      'content-type': 'application/grpc-web+proto',
      'x-grpc-web': '1',
      'x-user-agent': 'dottie-dashboard',
      authorization: 'Bearer ' + token,
    },
    body: framed.buffer.slice(framed.byteOffset, framed.byteOffset + framed.byteLength) as ArrayBuffer,
    signal,
  });
  if (res.status === 404 || res.status === 401) throw new GrpcError(5, 'not_found');
  if (!res.ok) throw new Error(method + ' → HTTP ' + res.status);
  const hdrStatus = res.headers.get('grpc-status');
  const hdrMsg = res.headers.get('grpc-message') ?? '';
  if (hdrStatus != null && hdrStatus !== '0') {
    throw new GrpcError(parseInt(hdrStatus, 10) || 2, hdrMsg || 'not_found');
  }
  if (!res.body) throw new Error(method + ': empty body');

  const reader = res.body.getReader();
  let buf: Uint8Array<ArrayBuffer> = new Uint8Array(0);
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done || !value) break;
      buf = appendBuf(buf, value);
      // Drain complete frames.
      while (buf.length >= 5) {
        const flags = buf[0]!;
        const len = new DataView(buf.buffer, buf.byteOffset + 1, 4).getUint32(0, false);
        if (buf.length < 5 + len) break;
        const payload = buf.subarray(5, 5 + len);
        buf = new Uint8Array(buf.subarray(5 + len));
        if ((flags & 0x80) !== 0) {
          const text = new TextDecoder().decode(payload);
          let status = 0;
          let message = '';
          for (const line of text.split(/\r?\n/)) {
            const c = line.indexOf(':');
            if (c < 0) continue;
            const k = line.slice(0, c).trim().toLowerCase();
            const v = line.slice(c + 1).trim();
            if (k === 'grpc-status') status = parseInt(v, 10) || 0;
            if (k === 'grpc-message') message = decodeURIComponent(v.replace(/\+/g, ' '));
          }
          if (status !== 0) throw new GrpcError(status, message);
          return;
        }
        const json = decodeJsonBody(new Uint8Array(payload));
        yield JSON.parse(json || 'null') as T;
      }
    }
  } finally {
    reader.releaseLock();
  }
}
