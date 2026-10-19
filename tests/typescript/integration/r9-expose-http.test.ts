/**
 * R9 expose by upcall over real (loopback, relay-less) nodes, binding side
 * only: a JavaScript HTTP handler (`Expose.exposeHttp`) behind its
 * authorizer, the request head and bodies (inline and streamed) mapped both
 * ways, the response shapes JS may return, handler failures and a body that
 * fails mid-way, the JS pump stopping when the consumer leaves, the request
 * cap reaching native, and an edge's route policy (`Expose.serveRoutes`).
 * The relay semantics (HEAD, 501/502/504, timeouts, abandoned upcalls, size
 * and chunk limits, failure modes) are tested once in Rust:
 * aster/tests/it/expose_foreign.rs and ffi/tests/expose.rs.
 */

import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import {
  EdgeRouter,
  HttpBodyError,
  HttpBodyStream,
  HttpRequest,
  Node,
  Upcalls,
  relayHttp,
  requestRoute,
  type Expose,
  type HttpResponse,
  type NodeId,
  type RouteSpec,
  type UpcallContext,
} from '@aster-rpc/aster';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;
const HTTP_ALPN = 'aster-expose/r9';
const EDGE_ALPN = 'aster-expose/r9-edge';
/** 200 KiB: over `inlineRequestMax` (64 KiB), so it streams. */
const BIG = Buffer.from(Array.from({ length: 256 * 800 }, (_, i) => i % 256));

async function ready(node: Node): Promise<void> {
  for (let i = 0; i < 200 && node.directAddresses().length === 0; i++) {
    await new Promise((r) => setTimeout(r, 10));
  }
}

async function until<T>(what: string, f: () => T | undefined | false, ms = 10_000): Promise<T> {
  const deadline = Date.now() + ms;
  for (;;) {
    const v = f();
    if (v !== undefined && v !== false) return v;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}

const sum = (b: Uint8Array) => b.reduce((a, x) => a + x, 0);

describe('expose HTTP handlers by upcall', () => {
  let host: Node;
  let peer: Node;
  let stranger: Node;
  let upcalls: Upcalls;
  let expose: Expose;
  let server: { close(): void };
  let conn: Awaited<ReturnType<Node['connect']>>;

  const seen: HttpRequest[] = [];
  const contexts: UpcallContext[] = [];
  const asked: Array<{ serviceId: string; peer: NodeId }> = [];
  const state = { streamClosed: false, streamWrites: 0 };

  const app = async (request: HttpRequest, ctx: UpcallContext): Promise<HttpResponse | string> => {
    seen.push(request);
    contexts.push(ctx);
    switch (request.uri) {
      case '/hello':
        return {
          headers: { 'Content-Type': 'text/plain', 'x-peer': String(request.peer), 'x-multi': ['a', 'b'] },
          body: `hi ${request.method}`,
        };
      case '/echo': {
        const body = await request.readBody();
        return { status: 201, headers: [['x-len', String(body.length)]], body };
      }
      case '/stream': {
        // Streamed request in, streamed response out (async generator), with trailers.
        const data = await request.readBody();
        async function* chunks() {
          for (let i = 0; i < data.length; i += 50_000) yield data.subarray(i, i + 50_000);
        }
        return { headers: { 'x-sum': String(sum(data)) }, body: chunks(), trailers: { 'x-t': '1' } };
      }
      case '/list':
        return { body: [Buffer.from('a'), 'b', new Uint8Array([99])] };
      case '/text':
        return 'plain text';
      case '/boom':
        throw new Error('handler exploded');
      case '/bad':
        return { not: 'a response' } as unknown as HttpResponse;
      case '/midway': {
        async function* failing() {
          yield 'partial';
          throw new Error('mid-body failure');
        }
        return { body: failing() };
      }
      case '/forever': {
        // A response that never ends; notices the consumer going away at its next write.
        async function* endless() {
          try {
            for (;;) {
              state.streamWrites++;
              yield Buffer.alloc(32 * 1024, 7);
              await new Promise((r) => setTimeout(r, 5));
            }
          } finally {
            state.streamClosed = true;
          }
        }
        return { body: endless() };
      }
    }
    return { status: 404 };
  };

  beforeAll(async () => {
    host = await Node.start({ ...OFFLINE, alpns: [HTTP_ALPN] });
    peer = await Node.start(OFFLINE);
    stranger = await Node.start(OFFLINE);
    for (const n of [host, peer, stranger]) await ready(n);
    peer.native.addNodeAddr(host.native);
    stranger.native.addNodeAddr(host.native);

    upcalls = new Upcalls({ policies: { expose_http: { timeoutMs: 1_000 } } });
    expose = host.expose({ upcalls });
    expose.exposeHttp('app', app, {
      authorize: ({ serviceId, peer: who }) => {
        asked.push({ serviceId, peer: who });
        return who === peer.id;
      },
      maxRequestBody: 1 << 20,
    });
    server = host.serveExpose(expose, [HTTP_ALPN]);
    conn = await peer.connect(host.addr, HTTP_ALPN);
  });

  afterAll(async () => {
    conn?.close(0, 'done');
    server?.close();
    expose?.close();
    upcalls?.close();
    await Promise.all([host, peer, stranger].map((n) => n?.close()));
  });

  it('answers a small request inline, with header handling and the request head', async () => {
    const resp = await relayHttp(conn, 'app', { uri: '/hello', headers: { 'x-q': '1' } });
    expect(resp.status).toBe(200);
    expect(resp.body.toString()).toBe('hi GET');
    expect(resp.header('content-type')).toBe('text/plain');
    expect(resp.header('X-Peer')).toBe(peer.id);
    expect(resp.headers.filter((h) => h.name === 'x-multi').map((h) => h.value.toString())).toEqual(['a', 'b']);

    const req = seen[seen.length - 1];
    expect(req).toBeInstanceOf(HttpRequest);
    expect([req.serviceId, req.peer, req.method, req.uri]).toEqual(['app', peer.id, 'GET', '/hello']);
    expect(req.header('x-q')).toBe('1');
    expect(req.version).toBe('HTTP/1.1');
    expect(req.streamId).toBeGreaterThan(0);
    expect(Buffer.isBuffer(req.body)).toBe(false); // no declared body: streamed (reads end at once)
    expect(contexts[contexts.length - 1].kind).toBe('expose_http');
  });

  it('delivers a small declared body inline and a large one streamed, in both directions', async () => {
    const small = await relayHttp(conn, 'app', {
      method: 'POST',
      uri: '/echo',
      headers: { 'content-length': '5' },
      body: 'hello',
    });
    expect([small.status, small.body.toString(), small.header('x-len')]).toEqual([201, 'hello', '5']);
    expect(Buffer.isBuffer(seen[seen.length - 1].body)).toBe(true);
    expect(seen[seen.length - 1].body).toEqual(Buffer.from('hello'));

    const big = await relayHttp(conn, 'app', { method: 'PUT', uri: '/stream', body: BIG });
    const bigReq = seen[seen.length - 1];
    expect(bigReq.body).toBeInstanceOf(HttpBodyStream);
    expect(bigReq.body).toBe(bigReq.handle);
    expect(big.status).toBe(200);
    expect(big.body.equals(BIG)).toBe(true);
    expect(big.header('x-sum')).toBe(String(sum(BIG)));

    // A large body declared with its length streams too.
    const declared = await relayHttp(conn, 'app', {
      method: 'POST',
      uri: '/echo',
      headers: { 'content-length': String(BIG.length) },
      body: BIG,
    });
    expect(declared.body.equals(BIG)).toBe(true);
    expect(seen[seen.length - 1].body).toBeInstanceOf(HttpBodyStream);
  });

  it('streams response bodies from sync iterables; a bare string is a 200', async () => {
    expect((await relayHttp(conn, 'app', { uri: '/list' })).body.toString()).toBe('abc');
    const text = await relayHttp(conn, 'app', { uri: '/text' });
    expect([text.status, text.body.toString()]).toEqual([200, 'plain text']);
    expect((await relayHttp(conn, 'app', { uri: '/nope' })).status).toBe(404);
  });

  it('turns handler failures into 5xx without leaking the message', async () => {
    const boom = await relayHttp(conn, 'app', { uri: '/boom' });
    expect(boom.status).toBe(500);
    expect(boom.body.toString()).not.toContain('exploded');
    expect((await relayHttp(conn, 'app', { uri: '/bad' })).status).toBe(500);
  });

  it('reaches the consumer as an error, never a truncated body, when a response fails mid-body', async () => {
    await expect(relayHttp(conn, 'app', { uri: '/midway' })).rejects.toBeInstanceOf(Error);
    // The service keeps working afterwards.
    expect((await relayHttp(conn, 'app', { uri: '/hello' })).status).toBe(200);
  });

  it('closes a streaming handler when the consumer goes away mid-response', async () => {
    state.streamClosed = false;
    state.streamWrites = 0;
    const gone = await peer.connect(host.addr, HTTP_ALPN);
    const pending = relayHttp(gone, 'app', { uri: '/forever' }).catch((e: unknown) => e);
    await until('streaming started', () => state.streamWrites > 3 || undefined);
    gone.close(0, 'giving up');
    // The next write fails with `cancelled`, the pump stops and the generator's `finally` runs.
    await until('generator closed', () => state.streamClosed || undefined);
    expect(await pending).toBeInstanceOf(Error);
  });

  it('keeps the authorizer in front of the service', async () => {
    const before = asked.length;
    const other = await stranger.connect(host.addr, HTTP_ALPN);
    try {
      await expect(relayHttp(other, 'app', { uri: '/hello' })).rejects.toBeInstanceOf(Error);
    } finally {
      other.close(0, 'done');
    }
    expect(asked.slice(before)).toEqual([{ serviceId: 'app', peer: stranger.id }]);
  });

  it('answers 413 for a declared body over maxRequestBody without calling the handler', async () => {
    let calls = 0;
    expose.exposeHttp(
      'capped',
      () => {
        calls++;
        return 'ok';
      },
      { maxRequestBody: 1000 },
    );
    const resp = await relayHttp(conn, 'capped', {
      method: 'POST',
      headers: { 'content-length': '5000' },
      body: Buffer.alloc(5000, 1),
    });
    expect(resp.status).toBe(413);
    expect(calls).toBe(0);
    // A streamed body that outgrows the cap fails the read (`too_large`): the handler decides.
    expose.exposeHttp(
      'capped-stream',
      async (req) => {
        try {
          await req.readBody();
          return 'fits';
        } catch (e) {
          return { status: e instanceof HttpBodyError && e.kind === 'too_large' ? 413 : 500 };
        }
      },
      { maxRequestBody: 100_000 },
    );
    expect((await relayHttp(conn, 'capped-stream', { method: 'POST', body: BIG })).status).toBe(413);
    expose.unexpose('capped');
    expose.unexpose('capped-stream');
  });

  it('validates registration', () => {
    expect(() => expose.exposeHttp('x', null as never)).toThrow(TypeError);
    expect(() => expose.exposeHttp('x', () => 'x', { maxChunk: 0 })).toThrow(RangeError);
  });
});

describe('edge route policy by upcall', () => {
  it('grants what the policy answers: true, false, indices or a subset of the specs', async () => {
    await using edge = await Node.start({ ...OFFLINE, alpns: [EDGE_ALPN] });
    await using origin = await Node.start(OFFLINE);
    await ready(edge);
    origin.native.addNodeAddr(edge.native);

    const upcalls = new Upcalls();
    const requests: Array<{ peer: NodeId; metadata: string; specs: readonly RouteSpec[] }> = [];
    const router = new EdgeRouter();
    const expose = edge.expose({ upcalls });
    expose.serveRoutes(router, ({ peer, metadata, specs }) => {
      const meta = Buffer.from(metadata).toString();
      requests.push({ peer, metadata: meta, specs });
      if (meta === 'all') return true;
      if (meta === 'none') return false;
      if (meta === 'index') return [2];
      return specs.filter((s) => s.host.endsWith('.example'));
    });
    const server = edge.serveExpose(expose, [EDGE_ALPN]);
    const conn = await origin.connect(edge.addr, EDGE_ALPN);
    const specs: RouteSpec[] = [
      { host: 'a.example', port: 443, serviceId: 'web' },
      { host: 'evil.test', port: 443, protocol: 'http', serviceId: 'web' },
      { host: '', port: 5432, protocol: 'tcp', serviceId: 'db' },
    ];
    const full = specs.map((s) => ({ port: 0, protocol: 'http', ...s }));
    try {
      const reg = await requestRoute(conn, specs, Buffer.from('tok'));
      expect(reg.granted).toEqual([full[0]]);
      expect(requests[0]).toEqual({ peer: origin.id, metadata: 'tok', specs: full });
      expect(router.lookup('a.example', 443)).toEqual({ serviceId: 'web', protocol: 'http', node: origin.id });
      expect(router.lookup('evil.test', 443)).toBeNull();
      await reg.close();
      expect(reg.closed).toBe(true);
      await until('route evicted', () => router.lookup('a.example', 443) === null || undefined);

      expect((await requestRoute(conn, specs, Buffer.from('all'))).granted).toEqual(full);
      expect((await requestRoute(conn, specs, Buffer.from('none'))).granted).toEqual([]);
      expect((await requestRoute(conn, specs, Buffer.from('index'))).granted).toEqual([full[2]]);
      expect(router.lookup('', 5432)).toMatchObject({ serviceId: 'db', protocol: 'tcp' });
      router.unregister('', 5432);
      expect(router.lookup('', 5432)).toBeNull();

    } finally {
      conn.close(0, 'done');
      server.close();
      expose.close();
      upcalls.close();
    }
  });

  it('grants every requested route without a policy', async () => {
    await using edge = await Node.start({ ...OFFLINE, alpns: [EDGE_ALPN] });
    await using origin = await Node.start(OFFLINE);
    await ready(edge);
    origin.native.addNodeAddr(edge.native);
    const router = new EdgeRouter();
    using expose = edge.expose();
    expose.serveRoutes(router);
    const server = edge.serveExpose(expose, [EDGE_ALPN]);
    const conn = await origin.connect(edge.addr, EDGE_ALPN);
    try {
      await using reg = await requestRoute(conn, [{ host: 'open.example', port: 80, serviceId: 'web' }]);
      expect(reg.granted).toHaveLength(1);
      expect(router.lookup('open.example', 80)?.node).toBe(origin.id);
    } finally {
      conn.close(0, 'done');
      server.close();
    }
  });
});
