/**
 * The streaming consumer relay (`relayHttpStream` / `relayHttpStreaming`),
 * end to end over real (loopback, relay-less) nodes: bodies streamed both
 * ways with bounded reads, every failure path (a throwing upload iterator,
 * an AbortSignal, a cancelled response handle, an aborting response, an
 * unknown service, an early close) failing loudly and never truncating, and
 * a handler proxying the relayed response through a third node.
 *
 * The handler's bookkeeping logs outcomes in `finally`/after the retry so a
 * reset landing during a retry is still recorded.
 */

import { describe, expect, it } from 'vitest';
import {
  HttpBodyError,
  HttpRelayBody,
  HttpUpload,
  Node,
  Upcalls,
  relayHttpStream,
  relayHttpStreaming,
  type HttpHandler,
  type HttpRequest,
  type HttpResponse,
  type HttpResponseBody,
} from '@aster-rpc/aster';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;
const ALPN = 'aster-expose/r9-stream';
/** 200 KiB. */
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

/** Fail (rather than hang the test) when `p` takes too long. */
function within<T>(p: Promise<T>, ms = 15_000): Promise<T> {
  let timer: ReturnType<typeof setTimeout>;
  return Promise.race([
    p,
    new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error('HANG: operation did not settle')), ms);
    }),
  ]).finally(() => clearTimeout(timer));
}

type LogEntry = readonly [string, ...unknown[]];

/** The handler side: records what each request body did and how response writes ended. */
class Streamer {
  readonly log: LogEntry[] = [];

  readonly handle: HttpHandler = async (request: HttpRequest): Promise<HttpResponse> => {
    this.log.push(['start', request.uri]);
    if (request.uri.startsWith('/echo')) {
      let data: Buffer;
      try {
        data = await request.readBody();
      } catch (e) {
        // Sticky: a retry fails too, rather than ending cleanly. Log the
        // outcome whatever happens meanwhile.
        const kind = e instanceof HttpBodyError ? e.kind : `other:${String(e)}`;
        let again = 'unknown';
        try {
          await request.handle.read();
          again = 'ok';
        } catch (e2) {
          again = e2 instanceof HttpBodyError ? e2.kind : `other:${String(e2)}`;
        } finally {
          this.log.push(['body failed', kind, again, request.uri]);
        }
        throw e;
      }
      this.log.push(['body', data.length]);
      async function* back() {
        for (let i = 0; i < data.length; i += 7000) yield data.subarray(i, i + 7000);
      }
      return { headers: { 'x-len': String(data.length) }, body: back() };
    }
    if (request.uri.startsWith('/early')) {
      // Answer without reading the body.
      return { status: 413, body: 'too big' };
    }
    if (request.uri.startsWith('/duplex')) {
      // Head first, then echo each request chunk as it arrives. The outcome
      // is logged in `finally`, keyed by uri, so tests can scope to a request.
      const log = this.log;
      const uri = request.uri;
      async function* echo() {
        let outcome: LogEntry = ['duplex cancelled', uri];
        try {
          for await (const chunk of request.handle) yield chunk;
          outcome = ['duplex done', uri];
        } catch (e) {
          outcome = ['duplex failed', e instanceof HttpBodyError ? e.kind : `other:${String(e)}`, uri];
          throw e;
        } finally {
          log.push(outcome);
        }
      }
      return { body: echo() };
    }
    if (request.uri === '/abort') {
      async function* failing() {
        yield Buffer.alloc(600, 120);
        await new Promise((r) => setTimeout(r, 50));
        throw new Error('origin failed mid-body');
      }
      return { body: failing() };
    }
    if (request.uri === '/forever') {
      const log = this.log;
      async function* endless() {
        try {
          for (;;) yield Buffer.alloc(16384, 121);
        } finally {
          log.push(['forever closed']);
        }
      }
      return { body: endless() };
    }
    return { status: 404 };
  };

  started(uri: string): boolean {
    return this.log.some((e) => e[0] === 'start' && e[1] === uri);
  }

  /** The `/duplex` outcomes of the request for `uri`, in order. */
  duplex(uri: string): LogEntry[] {
    return this.log.filter((e) => e[0].startsWith('duplex') && e[e.length - 1] === uri);
  }

  /** The failed-body outcomes of the requests for `uri`. */
  failures(uri: string): LogEntry[] {
    return this.log.filter((e) => e[0] === 'body failed' && e[3] === uri);
  }
}

interface Pair {
  host: Node;
  peer: Node;
  conn: Awaited<ReturnType<Node['connect']>>;
  close(): Promise<void>;
}

async function pair(services: Record<string, HttpHandler>): Promise<Pair> {
  const host = await Node.start({ ...OFFLINE, alpns: [ALPN] });
  const peer = await Node.start(OFFLINE);
  for (const n of [host, peer]) await ready(n);
  peer.native.addNodeAddr(host.native);
  const upcalls = new Upcalls({ policies: { expose_http: { timeoutMs: 10_000 } } });
  const expose = host.expose({ upcalls });
  for (const [name, handler] of Object.entries(services)) expose.exposeHttp(name, handler);
  const server = host.serveExpose(expose, [ALPN]);
  const conn = await peer.connect(host.addr, ALPN);
  return {
    host,
    peer,
    conn,
    async close() {
      conn.close(0, 'done');
      server.close();
      expose.close();
      upcalls.close();
      await Promise.all([host.close(), peer.close()]);
    },
  };
}

describe('streaming consumer relay', () => {
  it('streams both ways: bounded reads, bytes / iterable / empty bodies, for await, the low-level API', async () => {
    const app = new Streamer();
    const p = await pair({ app: app.handle });
    try {
      // Async-generator upload; bounded reads of the streamed response.
      async function* upload() {
        for (let i = 0; i < BIG.length; i += 30_000) yield BIG.subarray(i, i + 30_000);
      }
      const resp = await within(
        relayHttpStream(p.conn, 'app', { method: 'PUT', uri: '/echo', body: upload(), maxChunk: 16384 }),
      );
      expect(resp.status).toBe(200);
      expect(resp.header('x-len')).toBe(String(BIG.length));
      expect(resp.body).toBeInstanceOf(HttpRelayBody);
      expect(resp.body.maxChunk).toBe(16384);
      const got: Buffer[] = [];
      for (;;) {
        const chunk = await resp.body.read(5000);
        if (chunk === null) break;
        expect(chunk.length).toBeGreaterThan(0);
        expect(chunk.length).toBeLessThanOrEqual(5000);
        got.push(chunk);
      }
      expect(Buffer.concat(got).equals(BIG)).toBe(true);
      expect(await resp.body.read()).toBeNull(); // the end is sticky
      expect(await resp.body.read()).toBeNull();
      expect(app.log).toContainEqual(['body', BIG.length]);

      // Buffer, text, a sync iterable of mixed chunks, an async iterable and no body; readBody().
      const bodies: Array<[HttpResponseBody, string]> = [
        [Buffer.from('abc'), 'abc'],
        ['abc', 'abc'],
        [new Uint8Array([97, 98, 99]), 'abc'],
        [['a', Buffer.from('b'), new Uint8Array([99])], 'abc'],
        [
          (async function* () {
            yield 'a';
            yield 'bc';
          })(),
          'abc',
        ],
        [undefined, ''],
      ];
      for (const [body, want] of bodies) {
        const r = await within(relayHttpStream(p.conn, 'app', { method: 'POST', uri: '/echo', body }));
        expect((await r.readBody()).toString()).toBe(want);
      }

      // readBody with a limit: too_large closes the body.
      const big = await within(relayHttpStream(p.conn, 'app', { method: 'POST', uri: '/echo', body: BIG }));
      const err = await big.readBody(1000).catch((e: unknown) => e);
      expect(err).toBeInstanceOf(HttpBodyError);
      expect((err as HttpBodyError).kind).toBe('too_large');
      expect(big.body.closed).toBe(true);

      // for await over the body.
      const loop = await within(relayHttpStream(p.conn, 'app', { method: 'POST', uri: '/echo', body: BIG }));
      const parts: Buffer[] = [];
      for await (const c of loop.body) parts.push(c);
      expect(Buffer.concat(parts).equals(BIG)).toBe(true);

      // The low-level form: write the upload yourself.
      const { upload: up, response } = relayHttpStreaming(p.conn, 'app', {
        method: 'POST',
        uri: '/echo',
        headers: { 'x-a': '1' },
      });
      expect(up).toBeInstanceOf(HttpUpload);
      expect(up.closed).toBe(false);
      expect(up.maxChunk).toBe(1 << 20);
      const waiting = response.wait();
      await up.write('hello ');
      expect(typeof up.tryWrite('world')).toBe('boolean');
      up.finish();
      expect(up.closed).toBe(true);
      expect(() => up.finish()).toThrow(HttpBodyError);
      try {
        up.finish();
      } catch (e) {
        expect((e as HttpBodyError).kind).toBe('finished');
      }
      const low = await within(waiting);
      expect((await low.readBody()).toString().startsWith('hello ')).toBe(true);

      expect(() => relayHttpStreaming(p.conn, 'app', { method: 'BAD METHOD' })).toThrow();
      expect(() => relayHttpStreaming(p.conn, 'app', { maxChunk: 0 })).toThrow();
    } finally {
      await p.close();
    }
  });

  it('never truncates: failures reset the request or reject, and the right error wins', async () => {
    const app = new Streamer();
    const p = await pair({ app: app.handle });
    const gates: Array<() => void> = [];
    const parked = () =>
      new Promise<void>((resolve) => {
        const t = setTimeout(resolve, 30_000);
        gates.push(() => {
          clearTimeout(t);
          resolve();
        });
      });
    try {
      // An upload iterator that throws: its own error propagates (not the
      // relay reset it caused), and the handler sees a failed request body,
      // sticky, never a short one.
      const sourceError = new RangeError('upload source failed');
      async function* broken() {
        yield Buffer.alloc(50_000, 122);
        await until('handler started', () => app.started('/echo?iter') || undefined);
        throw sourceError;
      }
      const thrown = await within(relayHttpStream(p.conn, 'app', { method: 'PUT', uri: '/echo?iter', body: broken() })).catch(
        (e: unknown) => e,
      );
      expect(thrown).toBe(sourceError);
      await until('handler saw the failed body', () => app.failures('/echo?iter').length === 1 || undefined);
      const [, kind, again] = app.failures('/echo?iter')[0];
      expect(['read', 'cancelled']).toContain(kind);
      expect(['read', 'cancelled']).toContain(again);

      // A synchronous iterator that throws wins too.
      const syncError = new TypeError('sync source failed');
      const syncThrown = await within(
        relayHttpStream(p.conn, 'app', {
          method: 'PUT',
          uri: '/echo?sync',
          body: (function* () {
            yield Buffer.alloc(1000, 1);
            throw syncError;
          })(),
        }),
      ).catch((e: unknown) => e);
      expect(syncThrown).toBe(syncError);
      // (The reset may land before the handler even starts: nothing to wait for.)

      // A chunk of a wrong type is the caller's error, too.
      const wrongType = await within(
        relayHttpStream(p.conn, 'app', { method: 'PUT', uri: '/echo?type', body: [42 as unknown as string] }),
      ).catch((e: unknown) => e);
      expect(wrongType).toBeInstanceOf(TypeError);

      // AbortSignal mid-upload resets the request and rejects with its reason.
      const stop = new AbortController();
      let returned = false;
      const slow: AsyncIterable<Buffer> = {
        [Symbol.asyncIterator]() {
          let n = 0;
          return {
            async next() {
              if (n++ === 0) return { done: false, value: Buffer.alloc(50_000, 123) };
              await parked();
              return { done: true, value: undefined };
            },
            async return() {
              returned = true;
              return { done: true, value: undefined };
            },
          };
        },
      };
      const call = relayHttpStream(p.conn, 'app', { method: 'PUT', uri: '/echo?signal', body: slow, signal: stop.signal });
      const callResult = call.catch((e: unknown) => e);
      await until('handler started', () => app.started('/echo?signal') || undefined);
      const reason = new Error('caller gave up');
      stop.abort(reason);
      expect(await within(callResult)).toBe(reason);
      await until('aborted call failed the body', () => app.failures('/echo?signal').length === 1 || undefined);
      await until('source asked to stop', () => returned || undefined);
      expect(await within(relayHttpStream(p.conn, 'app', { signal: AbortSignal.abort(reason) }).catch((e: unknown) => e))).toBe(
        reason,
      );

      // Low level: cancelling only the response handle resets the request,
      // even with the upload still open.
      const { upload, response } = relayHttpStreaming(p.conn, 'app', { method: 'PUT', uri: '/echo?cancel' });
      const waiting = response.wait().catch((e: unknown) => e);
      await upload.write(Buffer.alloc(50_000, 124));
      await until('handler started', () => app.started('/echo?cancel') || undefined);
      response.cancel();
      expect(await within(waiting)).toBeInstanceOf(Error);
      await until('cancelled handle failed the body', () => app.failures('/echo?cancel').length === 1 || undefined);
      await until('upload sees the reset', () => upload.closed || undefined);
      const gone = await upload.write(Buffer.from('more')).catch((e: unknown) => e);
      expect(gone).toBeInstanceOf(HttpBodyError);
      expect(['cancelled', 'finished']).toContain((gone as HttpBodyError).kind);
      try {
        upload.abort();
      } catch {
        /* already over */
      }

      // An unknown service rejects the call; it never hangs the upload.
      const unknown = await within(relayHttpStream(p.conn, 'nope', { method: 'PUT', uri: '/', body: BIG })).catch(
        (e: unknown) => e,
      );
      expect(unknown).toBeInstanceOf(Error);
      expect(unknown).not.toBeInstanceOf(HttpBodyError);
      expect((unknown as Error).message).not.toMatch(/HANG/);

      // A response that fails partway rejects the read, every time (code 7).
      const resp = await within(relayHttpStream(p.conn, 'app', { uri: '/abort' }));
      const failed = await (async () => {
        try {
          for await (const _ of resp.body) void _;
        } catch (e) {
          return e;
        }
        return undefined;
      })();
      expect(failed).toBeInstanceOf(HttpBodyError);
      expect([(failed as HttpBodyError).kind, (failed as HttpBodyError).code]).toEqual(['read', 7]);
      for (let i = 0; i < 2; i++) {
        const again2 = await resp.body.read().catch((e: unknown) => e);
        expect(again2).toBeInstanceOf(HttpBodyError);
        expect((again2 as HttpBodyError).kind).toBe('read');
      }
      const failedBody = await (await within(relayHttpStream(p.conn, 'app', { uri: '/abort' }))).readBody().catch((e: unknown) => e);
      expect(failedBody).toBeInstanceOf(HttpBodyError);

      // Closing the body early stops the handler's endless stream.
      const endless = await within(relayHttpStream(p.conn, 'app', { uri: '/forever' }));
      {
        await using body = endless.body;
        expect(await body.read(100)).toBeTruthy();
      }
      expect(endless.body.closed).toBe(true);
      const closed = await endless.body.read().catch((e: unknown) => e);
      expect(closed).toBeInstanceOf(HttpBodyError);
      expect((closed as HttpBodyError).kind).toBe('cancelled');
      await until('handler stream closed', () => app.log.some((e) => e[0] === 'forever closed') || undefined);

      // Leaving a `for await` early closes the body too.
      app.log.length = 0;
      const leaver = await within(relayHttpStream(p.conn, 'app', { uri: '/forever' }));
      for await (const c of leaver.body) {
        expect(c.length).toBeGreaterThan(0);
        break;
      }
      expect(leaver.body.closed).toBe(true);
      await until('handler stream closed again', () => app.log.some((e) => e[0] === 'forever closed') || undefined);

      // No handler ever took a failed upload for a complete (short) one.
      expect(app.log.filter((e) => e[0] === 'body')).toEqual([]);
    } finally {
      for (const g of gates) g();
      await p.close();
    }
  });

  it('is full duplex: early answers, ping-pong, post-head failures and closing mid-upload', async () => {
    const app = new Streamer();
    const p = await pair({ app: app.handle });
    try {
      // An early answer arrives while an endless upload is still going; the
      // upload then stops and its generator is closed.
      let closed = false;
      async function* endless() {
        try {
          for (;;) yield Buffer.alloc(65536, 117);
        } finally {
          closed = true;
        }
      }
      const early = await within(relayHttpStream(p.conn, 'app', { method: 'PUT', uri: '/early', body: endless() }));
      expect(early.status).toBe(413);
      expect((await within(early.readBody())).toString()).toBe('too big');
      await until('upload generator closed', () => closed || undefined);

      // Low-level ping-pong: each echo is read before the next write.
      {
        const { upload, response } = relayHttpStreaming(p.conn, 'app', { method: 'POST', uri: '/duplex?pingpong' });
        const resp = await within(response.wait());
        expect(resp.status).toBe(200);
        expect(upload.closed).toBe(false);
        for (let i = 0; i < 20; i++) {
          const ping = Buffer.from(`ping ${i}`);
          await within(upload.write(ping));
          let echo = Buffer.alloc(0);
          while (echo.length < ping.length) {
            const c = await within(resp.body.read());
            expect(c).not.toBeNull();
            echo = Buffer.concat([echo, c as Buffer]);
          }
          expect(echo.equals(ping)).toBe(true);
        }
        upload.finish();
        expect(await within(resp.body.read())).toBeNull();
        await until('duplex done', () => app.duplex('/duplex?pingpong').some((e) => e[0] === 'duplex done') || undefined);
      }

      // The same through relayHttpStream, with a generator driven by the
      // echoes it gets back.
      {
        const replies: Array<() => void> = [];
        let pending = 0;
        const gotReply = () =>
          pending > 0
            ? (pending--, Promise.resolve())
            : new Promise<void>((resolve) => {
                replies.push(() => (pending--, resolve()));
              });
        async function* chatty() {
          for (let i = 0; i < 5; i++) {
            yield Buffer.from(`msg ${i}`);
            await within(gotReply());
          }
        }
        const resp = await within(relayHttpStream(p.conn, 'app', { method: 'POST', uri: '/duplex?chatty', body: chatty() }));
        let got = '';
        for await (const chunk of resp.body) {
          got += chunk.toString();
          pending++;
          replies.shift()?.();
        }
        expect(got).toBe([0, 1, 2, 3, 4].map((i) => `msg ${i}`).join(''));
      }

      // A body iterator failing after the head: the handler sees a failed
      // body, the response read fails `read`, and the error is surfaced
      // through onUploadError (the call had already resolved).
      {
        const uri = '/duplex?flaky';
        const sourceError = new RangeError('source failed after the head');
        const surfaced: unknown[] = [];
        let release!: () => void;
        const released = new Promise<void>((resolve) => (release = resolve));
        async function* flaky() {
          yield Buffer.from('one');
          await released;
          throw sourceError;
        }
        const resp = await within(
          relayHttpStream(p.conn, 'app', { method: 'POST', uri, body: flaky(), onUploadError: (e) => surfaced.push(e) }),
        );
        expect((await within(resp.body.read()))?.toString()).toBe('one');
        expect(surfaced).toEqual([]);
        release();
        const failed = await within(
          (async () => {
            try {
              while ((await resp.body.read()) !== null);
            } catch (e) {
              return e;
            }
            return undefined;
          })(),
        );
        expect(failed).toBeInstanceOf(HttpBodyError);
        expect((failed as HttpBodyError).kind).toBe('read');
        await until('handler saw the failed body', () => app.duplex(uri).some((e) => e[0] === 'duplex failed') || undefined);
        await until('error surfaced', () => surfaced.length === 1 || undefined);
        expect(surfaced[0]).toBe(sourceError);
        expect(app.duplex(uri).some((e) => e[0] === 'duplex done')).toBe(false);
      }

      // Closing the response mid-upload makes the upload's writes fail
      // `cancelled`.
      {
        const { upload, response } = relayHttpStreaming(p.conn, 'app', { method: 'POST', uri: '/duplex?close' });
        const resp = await within(response.wait());
        await upload.write(Buffer.from('x'));
        expect((await within(resp.body.read()))?.toString()).toBe('x');
        resp.body.close();
        await until('upload stopped', () => upload.closed || undefined);
        const gone = await upload.write(Buffer.from('more')).catch((e: unknown) => e);
        expect(gone).toBeInstanceOf(HttpBodyError);
        expect((gone as HttpBodyError).kind).toBe('cancelled');
      }

      // The same through relayHttpStream: closing the body stops the pump
      // and closes the source generator.
      {
        let sourceClosed = false;
        async function* steady() {
          try {
            for (let i = 0; ; i++) {
              yield Buffer.from(`s${i}`);
              await new Promise((r) => setTimeout(r, 5));
            }
          } finally {
            sourceClosed = true;
          }
        }
        const resp = await within(relayHttpStream(p.conn, 'app', { method: 'POST', uri: '/duplex?close2', body: steady() }));
        expect(await within(resp.body.read())).toBeTruthy();
        resp.body.close();
        await until('source generator closed', () => sourceClosed || undefined);
      }
    } finally {
      await p.close();
    }
  });

  it('proxies a relayed response through a handler across three nodes', async () => {
    const origin = await Node.start({ ...OFFLINE, alpns: [ALPN] });
    const edge = await Node.start({ ...OFFLINE, alpns: [ALPN] });
    const client = await Node.start(OFFLINE);
    const upcalls: Upcalls[] = [];
    const resources: Array<{ close(): void }> = [];
    try {
      for (const n of [origin, edge, client]) await ready(n);
      edge.native.addNodeAddr(origin.native);
      client.native.addNodeAddr(edge.native);

      const app = new Streamer();
      const originUp = new Upcalls();
      upcalls.push(originUp);
      const originExpose = origin.expose({ upcalls: originUp });
      resources.push(originExpose);
      originExpose.exposeHttp('app', app.handle);
      resources.push(origin.serveExpose(originExpose, [ALPN]));
      const upstream = await edge.connect(origin.addr, ALPN);
      resources.push({ close: () => upstream.close(0, 'done') });

      const edgeUp = new Upcalls({ policies: { expose_http: { timeoutMs: 10_000 } } });
      upcalls.push(edgeUp);
      const edgeExpose = edge.expose({ upcalls: edgeUp });
      resources.push(edgeExpose);
      // Proxy by returning the relayed response, request body streamed through.
      edgeExpose.exposeHttp('proxy', (req) =>
        relayHttpStream(upstream, 'app', { method: req.method, uri: req.uri, body: req.body }),
      );
      // ... or just its body.
      edgeExpose.exposeHttp('proxy-body', async (req) => (await relayHttpStream(upstream, 'app', { uri: req.uri })).body);
      resources.push(edge.serveExpose(edgeExpose, [ALPN]));
      const conn = await client.connect(edge.addr, ALPN);
      resources.push({ close: () => conn.close(0, 'done') });

      const resp = await within(relayHttpStream(conn, 'proxy', { method: 'PUT', uri: '/echo', body: BIG }));
      expect(resp.status).toBe(200);
      expect(resp.header('x-len')).toBe(String(BIG.length));
      expect((await resp.readBody()).equals(BIG)).toBe(true);

      // Closing the body at the client stops the origin's endless stream through the edge.
      const endless = await within(relayHttpStream(conn, 'proxy', { uri: '/forever' }));
      expect(await endless.body.read(100)).toBeTruthy();
      endless.body.close();
      await until('origin stream closed through the proxy', () => app.log.some((e) => e[0] === 'forever closed') || undefined);

      // Returning only the body is a 200 with that body.
      const bare = await within(relayHttpStream(conn, 'proxy-body', { uri: '/abort' }));
      expect(bare.status).toBe(200);
      // The origin's response fails partway: the client sees an error, never a short body.
      await expect(bare.readBody()).rejects.toBeInstanceOf(Error);
    } finally {
      for (const r of resources.reverse()) r.close();
      for (const u of upcalls) u.close();
      await Promise.all([origin, edge, client].map((n) => n.close()));
    }
  });
});
