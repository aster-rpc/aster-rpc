/**
 * T4.0: the native `DispatchServer` prototype (dynamic-dispatch-design.md
 * §4.3) — JS handlers answering by call id, Rust routing — for every event
 * delivery variant (TSFN per event, TSFN batched, ring + await, ring + poll).
 *
 * Unary, server-stream (with backpressure), client-stream, deadline cancel
 * and peer cancel (connection closed mid-stream), each ending with an empty
 * call table: whatever happened to a call, Rust must have let go of it.
 *
 * The client is the TS `IrohTransport` over a real QUIC connection, with an
 * identity codec: payloads are opaque bytes to the server.
 */

import { randomBytes } from 'node:crypto';
import { describe, it, expect, beforeAll, afterAll, vi } from 'vitest';
import {
  DISPATCH_DELIVERIES,
  DispatchEventKind,
  DispatchFlags,
  DispatchLoop,
  finishCall,
  respondCall,
  sendFrame,
  submitWithBackpressure,
  type DispatchEvent,
  type DynamicServiceSpec,
  type NativeDispatchServer,
} from '../../../bindings/typescript/packages/aster/src/dispatch-server.ts';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.ts';
import * as nativeLoader from '../../../bindings/typescript/packages/aster/src/native.ts';
import { IrohTransport } from '../../../bindings/typescript/packages/aster/src/transport/iroh.ts';
import { ClientChain } from '../../../bindings/typescript/packages/aster/src/client-interceptors.ts';
import { CircuitBreakerInterceptor } from '../../../bindings/typescript/packages/aster/src/interceptors/circuit-breaker.ts';
import { RpcPattern } from '../../../bindings/typescript/packages/aster/src/types.ts';
import { RpcError, StatusCode } from '../../../bindings/typescript/packages/aster/src/status.ts';

const native = tryLoadNative() as any;
const hasDispatch = !!native?.DispatchServer;

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0', logLevel: 'warn' } as const;
const RPC_ALPN = Buffer.from('aster/1');
const h = (b: string) => b.repeat(32);

const ECHO_SERVICE: DynamicServiceSpec = {
  name: 'test.Echo',
  version: 1,
  methods: [
    { name: 'echo', pattern: 'unary', request: { hash: h('a1') }, response: { hash: h('b2') } },
    // A 64 KiB frame limit also sizes the response queue's byte budget
    // (eight maximum frames), so the stream below outruns it.
    {
      name: 'count',
      pattern: 'server_stream',
      request: { hash: h('a1') },
      response: { hash: h('b2') },
      limits: { max_frame_size: 64 * 1024 },
    },
    { name: 'sum', pattern: 'client_stream', request: { hash: h('a1') }, response: { hash: h('b2') } },
    {
      name: 'slow',
      pattern: 'server_stream',
      request: { hash: h('a1') },
      response: { hash: h('b2') },
      default_timeout: 0.3,
    },
    { name: 'hang', pattern: 'server_stream', request: { hash: h('a1') }, response: { hash: h('b2') } },
  ],
};
const [ECHO, COUNT, SUM, SLOW, HANG] = [0, 1, 2, 3, 4];

/** Opaque bytes in and out. */
const BYTES = { encode: (x: unknown) => x as Uint8Array, decode: (x: Uint8Array) => x };

/** Times a stream send found the response queue full (backpressure path). */
let queueFullHits = 0;

const u32 = (n: number) => {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(n);
  return b;
};

interface Fixture {
  loop: DispatchLoop;
  cancelled: Map<number, number>; // callId -> reason
  waiters: Map<number, () => void>;
  hangStarted: Promise<number>;
}

function handlers(fx: { cancelled: Map<number, number>; waiters: Map<number, () => void> }) {
  let hangStarted!: (id: number) => void;
  const started = new Promise<number>((r) => (hangStarted = r));
  const waitCancel = (id: number) =>
    new Promise<void>((r) => (fx.cancelled.has(id) ? r() : fx.waiters.set(id, r)));

  const serve = async (ev: DispatchEvent, srv: NativeDispatchServer) => {
    const id = ev.callId;
    const req = ev.request!;
    switch (ev.methodIdx) {
      case ECHO: {
        await respondCall(srv, id, Buffer.concat([Buffer.from('echo:'), req]));
        return;
      }
      case COUNT: {
        // n incompressible 4 KiB frames. The client stalls after the first,
        // so QUIC flow control backs up into the response queue (256 items)
        // and the QUEUE_FULL -> waitWritable -> retry path runs.
        const n = req.readUInt32LE(0);
        for (let i = 0; i < n; i++) {
          const frame = randomBytes(4 * 1024);
          frame.writeUInt32LE(i);
          let r = srv.send(id, frame);
          if (r > 0) {
            queueFullHits++;
            r = await submitWithBackpressure(srv, id, () => srv.send(id, frame));
          }
          if (r !== 0) return;
        }
        await finishCall(srv, id);
        return;
      }
      case SUM: {
        let total = 0;
        let items = 0;
        if (req.length) {
          total += req.readUInt32LE(0);
          items++;
        }
        if (!(ev.flags & DispatchFlags.FirstIsEnd)) {
          for (let b = await srv.recv(id); b !== null; b = await srv.recv(id)) {
            total += b.readUInt32LE(0);
            items++;
          }
        }
        await respondCall(srv, id, Buffer.from(`${items}:${total}`));
        return;
      }
      case SLOW:
      case HANG: {
        await sendFrame(srv, id, Buffer.from('first'));
        if (ev.methodIdx === HANG) hangStarted(id);
        await waitCancel(id);
        // The call is over: every further op reports it gone.
        expect(srv.isCancelled(id)).toBe(true);
        expect(srv.send(id, Buffer.from('late'))).toBeLessThan(0);
        return;
      }
    }
  };
  return { serve, started };
}

async function startFixture(delivery: (typeof DISPATCH_DELIVERIES)[number]): Promise<Fixture> {
  const cancelled = new Map<number, number>();
  const waiters = new Map<number, () => void>();
  const { serve, started } = handlers({ cancelled, waiters });
  const loop = await DispatchLoop.start({
    config: OFFLINE as any,
    services: [ECHO_SERVICE],
    delivery,
    builtinNodeInfo: false,
    onCall: (ev, srv) => {
      serve(ev, srv).catch((e) => srv.respond(ev.callId, Buffer.alloc(0), StatusCode.INTERNAL, String(e)));
    },
    onEvent: (ev) => {
      if (ev.kind === DispatchEventKind.Cancelled) {
        cancelled.set(ev.callId, ev.reason);
        waiters.get(ev.callId)?.();
      }
    },
  });
  return { loop, cancelled, waiters, hangStarted: started };
}

async function connect(address: string) {
  const node = await native.Node.start({ ...OFFLINE, alpns: [RPC_ALPN] });
  const t = native.asterTicketFromString(address);
  const conn = await node.connectNodeAddr(t.endpointId, RPC_ALPN, t.directAddrs, undefined);
  return { node, conn, transport: new IrohTransport(conn, BYTES as any) };
}

async function until(cond: () => boolean, ms = 5000): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error('timed out waiting');
    await new Promise((r) => setTimeout(r, 10));
  }
}

describe.skipIf(!hasDispatch).each(DISPATCH_DELIVERIES)('DispatchServer (%s)', (delivery) => {
  let fx: Fixture;
  let client: Awaited<ReturnType<typeof connect>>;

  beforeAll(async () => {
    fx = await startFixture(delivery);
    client = await connect(fx.loop.address);
  });

  afterAll(async () => {
    try { client?.conn.close(0, 'done'); } catch { /* ignore */ }
    await client?.node.close();
    await fx?.loop.close();
  });

  it('unary: concurrent calls answered by call id', async () => {
    const replies = await Promise.all(
      Array.from({ length: 64 }, (_, i) => client.transport.unary('test.Echo', 'echo', Buffer.from(`m${i}`))),
    );
    replies.forEach((r, i) => expect(Buffer.from(r as Uint8Array).toString()).toBe(`echo:m${i}`));
    await until(() => fx.loop.native.liveCalls() === 0);
  });

  it('server stream: frames in order, through response backpressure', async () => {
    queueFullHits = 0;
    const n = 2000; // 8 MiB: past the QUIC windows plus the response queue
    let i = 0;
    for await (const f of client.transport.serverStream('test.Echo', 'count', u32(n))) {
      expect(Buffer.from(f as Uint8Array).readUInt32LE(0)).toBe(i++);
      if (i === 1) await new Promise((r) => setTimeout(r, 300));
    }
    expect(i).toBe(n);
    expect(queueFullHits).toBeGreaterThan(0);
    await until(() => fx.loop.native.liveCalls() === 0);
  });

  it('client stream: first request on the event, the rest via recv', async () => {
    async function* reqs() {
      for (let i = 1; i <= 100; i++) yield u32(i);
    }
    const r = await client.transport.clientStream('test.Echo', 'sum', reqs());
    expect(Buffer.from(r as Uint8Array).toString()).toBe('100:5050');
    await until(() => fx.loop.native.liveCalls() === 0);
  });

  it('deadline: DEADLINE_EXCEEDED to the peer, Cancelled(deadline) to the handler', async () => {
    const seen: Uint8Array[] = [];
    let err: unknown;
    try {
      for await (const f of client.transport.serverStream('test.Echo', 'slow', u32(0))) seen.push(f as Uint8Array);
    } catch (e) {
      err = e;
    }
    expect(seen.length).toBe(1);
    expect(err).toBeInstanceOf(RpcError);
    expect((err as RpcError).code).toBe(StatusCode.DEADLINE_EXCEEDED);
    await until(() => [...fx.cancelled.values()].includes(2));
    await until(() => fx.loop.native.liveCalls() === 0);
  });

  it('peer cancel: connection closed mid-stream cancels the handler', async () => {
    const other = await connect(fx.loop.address);
    const it = other.transport.serverStream('test.Echo', 'hang', u32(0))[Symbol.asyncIterator]();
    const first = await it.next();
    expect(Buffer.from(first.value as Uint8Array).toString()).toBe('first');
    const id = await fx.hangStarted;
    other.conn.close(0, 'bye');
    await until(() => fx.cancelled.get(id) === 1);
    await until(() => fx.loop.native.liveCalls() === 0);
    await other.node.close();
  });
});

describe.skipIf(!hasDispatch)('DispatchServer ring delivery close', () => {
  it('wakes an idle nextBatch waiter on close and preserves its attached node', async () => {
    const node = await native.Node.start({ ...OFFLINE, alpns: [RPC_ALPN] });
    const server = await native.DispatchServer.serve(
      node,
      [JSON.stringify(ECHO_SERVICE)],
      { delivery: 'ring', ringCapacity: 16, builtinNodeInfo: false },
    );
    let timeout: ReturnType<typeof setTimeout> | undefined;
    try {
      const batch = server.nextBatch(1);
      // A locked consumer proves the idle pull has entered native code.
      // Closing before this point would miss the waiter/close lock ordering.
      await until(() => server.debugState()[1] === -1);
      const closed = server.close();
      const result = await Promise.race([
        Promise.all([batch, closed]).then(([events]) => events),
        new Promise((resolve) => { timeout = setTimeout(() => resolve('hung'), 5000); }),
      ]);
      expect(result).toEqual([]);
      expect(server.debugState().slice(0, 3)).toEqual([0, 0, 1]);
      expect(await server.nextBatch(1)).toEqual([]);
      // Reattaching requires the original node to remain usable after close.
      const replacement = await native.DispatchServer.serve(
        node,
        [JSON.stringify(ECHO_SERVICE)],
        { delivery: 'ring', ringCapacity: 16, builtinNodeInfo: false },
      );
      await replacement.close();
    } finally {
      clearTimeout(timeout);
      await node.close();
    }
  });

  it('an attached ring server ends live and unread calls on close', async () => {
    const node = await native.Node.start({ ...OFFLINE, alpns: [RPC_ALPN] });
    const server = await native.DispatchServer.serve(
      node,
      [JSON.stringify(ECHO_SERVICE)],
      { delivery: 'ring', ringCapacity: 16, builtinNodeInfo: false },
    );
    const peer = await native.Node.start({ ...OFFLINE, alpns: [RPC_ALPN] });
    const conn = await peer.connectNodeAddr(node.nodeId(), RPC_ALPN, node.directAddresses(), undefined);
    const transport = new IrohTransport(conn, BYTES as any);
    try {
      // Close may reject either call before its own await resumes. Attach both
      // outcome handlers when the calls are created, before any intervening await.
      const outcome = (p: Promise<unknown>) => p.then(
        () => 'answered',
        (e) => (e instanceof RpcError ? e.code : String(e)),
      );
      // One call taken by JS and left unanswered, one never taken.
      const taken = outcome(transport.unary('test.Echo', 'echo', Buffer.from('taken')));
      const first: DispatchEvent[] = await server.nextBatch(1);
      expect(first.map((e) => e.kind)).toEqual([DispatchEventKind.Call]);
      const unread = outcome(transport.unary('test.Echo', 'echo', Buffer.from('unread')));
      await until(() => server.liveCalls() === 2);
      await server.close();
      // The node stays up (attached), so only close can end these calls.
      // Keep the original post-close five-second outcome budget.
      const bounded = (p: Promise<unknown>) => Promise.race([
        p,
        new Promise((r) => setTimeout(() => r('hung'), 5000)),
      ]);
      expect(await bounded(taken)).toBe(StatusCode.INTERNAL);
      expect(await bounded(unread)).toBe(StatusCode.INTERNAL);
      expect(server.liveCalls()).toBe(0);
    } finally {
      try { conn.close(0, 'done'); } catch { /* ignore */ }
      await peer.close();
      await node.close();
    }
  });
});

describe.skipIf(!hasDispatch)('DispatchLoop JS wrapper attach window', () => {
  it('holds a real native callback until the JS wrapper attaches', async () => {
    let node: any;
    let peer: any;
    let release!: () => void;
    const held = new Promise<void>((resolve) => { release = resolve; });
    let ready!: () => void;
    let failReady!: (error: unknown) => void;
    const nativeReady = new Promise<void>((resolve, reject) => { ready = resolve; failReady = reject; });
    let delivered!: (event: DispatchEvent) => void;
    let failDelivery!: (error: unknown) => void;
    const callbackReturned = new Promise<DispatchEvent>((resolve, reject) => {
      delivered = resolve;
      failDelivery = reject;
    });
    const originalServe = native.DispatchServer.serve;
    // Run the real native server and callback, but hold its returned Promise.
    // This isolates the JS await boundary after native delivery is attached.
    const controlledServe = async (...args: any[]) => {
      const onEvents = args[3];
      args[3] = (events: DispatchEvent[]) => {
        try {
          onEvents(events);
        } catch (error) {
          failDelivery(error);
          return;
        }
        const call = events.find((event) => event.kind === DispatchEventKind.Call);
        if (call) delivered(call); // Acknowledge only after the real callback returns.
      };
      try {
        const server = await originalServe.apply(native.DispatchServer, args);
        ready();
        await held;
        return server;
      } catch (error) {
        failReady(error);
        throw error;
      }
    };
    // NAPI static methods are non-configurable, so wrap only the loader's
    // returned serve method; every event and server still comes from native.
    const spy = vi.spyOn(nativeLoader, 'loadNative').mockReturnValue({
      ...native,
      DispatchServer: { serve: controlledServe },
    } as any);
    let conn: any;
    let loop: DispatchLoop | undefined;
    let serving: Promise<DispatchLoop> | undefined;
    let timeout: ReturnType<typeof setTimeout> | undefined;
    let attached = false;
    const onCall = vi.fn((event: DispatchEvent, server: NativeDispatchServer) => {
      void respondCall(server, event.callId, Buffer.concat([Buffer.from('echo:'), event.request!]));
    });
    try {
      node = await native.Node.start({ ...OFFLINE, alpns: [RPC_ALPN] });
      peer = await native.Node.start({ ...OFFLINE, alpns: [RPC_ALPN] });
      serving = DispatchLoop.start({
        node,
        services: [ECHO_SERVICE],
        delivery: 'tsfn-batched',
        builtinNodeInfo: false,
        onCall,
      });
      void serving.then(() => { attached = true; }, () => {});
      await nativeReady;
      conn = await peer.connectNodeAddr(node.nodeId(), RPC_ALPN, node.directAddresses(), undefined);
      const transport = new IrohTransport(conn, BYTES as any);
      const reply = transport.unary('test.Echo', 'echo', Buffer.from('js-early')).then(
        (bytes) => Buffer.from(bytes as Uint8Array).toString(),
        (error) => ({ error }),
      );
      const event = await callbackReturned;
      expect(event.kind).toBe(DispatchEventKind.Call);
      expect(event.request!.toString()).toBe('js-early');
      expect(attached).toBe(false);
      expect(onCall).not.toHaveBeenCalled();
      release();
      loop = await serving;
      const result = await Promise.race([
        reply,
        new Promise((resolve) => { timeout = setTimeout(() => resolve('hung'), 5000); }),
      ]);
      expect(result).toBe('echo:js-early');
      expect(onCall).toHaveBeenCalledTimes(1);
      await until(() => loop!.native.liveCalls() === 0);
    } finally {
      clearTimeout(timeout);
      release();
      spy.mockRestore();
      try {
        loop ??= await serving;
        await loop?.close();
      } finally {
        try { conn?.close(0, 'done'); } catch { /* ignore */ }
        await peer?.close();
        await node?.close();
      }
    }
  });
});

describe.skipIf(typeof native?.DispatchServer?.debugArmAttach !== 'function')(
  'DispatchServer attach window',
  () => {
    it('delivers a call that arrives before serve() resolves', async () => {
      const node = await native.Node.start({ ...OFFLINE, alpns: [RPC_ALPN] });
      const peer = await native.Node.start({ ...OFFLINE, alpns: [RPC_ALPN] });
      native.DispatchServer.debugArmAttach();
      let conn: any;
      let loop: DispatchLoop | undefined;
      let serving: Promise<DispatchLoop> | undefined;
      try {
        serving = DispatchLoop.start({
          node,
          services: [ECHO_SERVICE],
          delivery: 'tsfn-batched',
          builtinNodeInfo: false,
          onCall: (ev, srv) => {
            void respondCall(srv, ev.callId, Buffer.concat([Buffer.from('echo:'), ev.request!]));
          },
        } as any);
        // Call while the server accepts but has not attached its call table.
        await until(() => native.DispatchServer.debugAttachState() === 0);
        conn = await peer.connectNodeAddr(node.nodeId(), RPC_ALPN, node.directAddresses(), undefined);
        const transport = new IrohTransport(conn, BYTES as any);
        const reply = transport.unary('test.Echo', 'echo', Buffer.from('early'));
        void reply.catch(() => {});
        // A live call in the held native gate proves arrival before attach.
        await until(() => native.DispatchServer.debugAttachState() === 1);
        native.DispatchServer.debugReleaseAttach();
        loop = await serving;
        const got = await Promise.race([
          reply.then((b: any) => Buffer.from(b).toString()),
          new Promise((r) => setTimeout(() => r('hung'), 5000)),
        ]);
        expect(got).toBe('echo:early');
      } finally {
        native.DispatchServer.debugReleaseAttach();
        try { conn?.close(0, 'done'); } catch { /* ignore */ }
        try {
          loop ??= await serving;
          await loop?.close();
        } finally {
          await peer.close();
          await node.close();
        }
      }
    });
  },
);

describe.skipIf(!hasDispatch)('DispatchServer bounded TSFN delivery', () => {
  it('closes an idle pump forwarder and clears its callback queue', async () => {
    const server = await native.DispatchServer.start(
      OFFLINE,
      [JSON.stringify(ECHO_SERVICE)],
      { delivery: 'tsfn-batched', ringCapacity: 16, builtinNodeInfo: false },
      () => {},
    );
    await server.close();
    expect(server.liveCalls()).toBe(0);
    expect(server.debugState().slice(0, 3)).toEqual([0, 0, 0]);
    await server.close();
  });

  it.skipIf(typeof native?.DispatchServer?.prototype?.debugFailDelivery !== 'function')(
    'fails live calls and releases queued ownership when callback delivery closes',
    async () => {
      let sawCall!: () => void;
      const delivered = new Promise<void>((resolve) => { sawCall = resolve; });
      const server = await native.DispatchServer.start(
        OFFLINE,
        [JSON.stringify(ECHO_SERVICE)],
        { delivery: 'tsfn-batched', ringCapacity: 16, builtinNodeInfo: false },
        (events: DispatchEvent[]) => {
          if (events.some((event) => event.kind === DispatchEventKind.Call)) sawCall();
        },
      );
      const client = await connect(server.address);
      try {
        const pending = client.transport.unary('test.Echo', 'echo', Buffer.from('fail-close'));
        await delivered;
        server.debugFailDelivery();
        await expect(pending).rejects.toMatchObject({ code: StatusCode.INTERNAL });
        await until(() => server.liveCalls() === 0);
        const state = server.debugState();
        expect(state[1]).toBe(0);
        expect(state[2]).toBe(0);
        expect(state[5]).toBeGreaterThan(0);
      } finally {
        try { client.conn.close(0, 'done'); } catch { /* ignore */ }
        await client.node.close();
        await server.close();
      }
    },
  );

  it('keeps a stalled callback within its admission bound and resumes afterward', async () => {
    let stalled = false;
    const batchSizes: number[] = [];
    let server: any;
    server = await native.DispatchServer.start(
      OFFLINE,
      [JSON.stringify(ECHO_SERVICE)],
      {
        delivery: 'tsfn-batched',
        ringCapacity: 16,
        maxForeignInflight: 16,
        builtinNodeInfo: false,
      },
      (events: DispatchEvent[]) => {
        batchSizes.push(events.length);
        if (!stalled) {
          stalled = true;
          Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 250);
        }
        for (const ev of events) {
          if (ev.kind === DispatchEventKind.Call) {
            server.respond(ev.callId, ev.request ?? Buffer.alloc(0));
          }
        }
      },
    );
    const client = await connect(server.address);
    try {
      const results = await Promise.allSettled(
        Array.from({ length: 20 }, async (_, i) => {
          let timeout: ReturnType<typeof setTimeout>;
          try {
            return await Promise.race([
              client.transport.unary('test.Echo', 'echo', Buffer.from('bounded-' + i)),
              new Promise<never>((_, reject) => {
                timeout = setTimeout(() => reject(new Error('dispatch timed out')), 5000);
              }),
            ]);
          } finally {
            clearTimeout(timeout!);
          }
        }),
      );
      expect(results.filter((r) => r.status === 'rejected' && String(r.reason).includes('dispatch timed out'))).toHaveLength(0);
      expect(results.some((r) => r.status === 'fulfilled')).toBe(true);
      expect(stalled).toBe(true);
      await until(() => server.liveCalls() === 0);
      expect(batchSizes.length).toBeGreaterThan(0);
      expect(Math.max(...batchSizes)).toBeLessThanOrEqual(16);
      expect(server.debugState()[1]).toBe(0);
      expect(server.debugState()[7]).toBeLessThanOrEqual(16);

      const resumed = await client.transport.unary('test.Echo', 'echo', Buffer.from('resumed'));
      expect(Buffer.from(resumed as Uint8Array).toString()).toBe('resumed');
    } finally {
      try { client.conn.close(0, 'done'); } catch { /* ignore */ }
      await client.node.close();
      await server.close();
    }
  });
});


describe.skipIf(!hasDispatch)('native breaker aftermath', () => {
  it.each(['later_refusal', 'cancelled_probe'])('%s allows a later healthy native probe', async (aftermath) => {
    let release!: () => void;
    const gate = new Promise<void>(r => { release = r; });
    const seen: string[] = [];
    const loop = await DispatchLoop.start({
      config: OFFLINE as any, services: [ECHO_SERVICE], delivery: 'ring', builtinNodeInfo: false,
      onCall: (ev, srv) => {
        const tag = Buffer.from(ev.request!).toString();
        seen.push(tag);
        if (tag === 'seed') return respondCall(srv, ev.callId, Buffer.alloc(0), StatusCode.UNAVAILABLE, 'seed');
        if (tag === 'hold') {
          return gate.then(() => respondCall(srv, ev.callId, Buffer.from('released'))).catch(() => {});
        }
        return respondCall(srv, ev.callId, Buffer.from(tag));
      },
    });
    let peer: Awaited<ReturnType<typeof connect>> | undefined;
    const abort = new AbortController();
    try {
      peer = await connect(loop.address);
      const first = new CircuitBreakerInterceptor({failureThreshold: 1, resetTimeoutMs: 0});
      const second = new CircuitBreakerInterceptor({failureThreshold: 1, resetTimeoutMs: 150});
      const chain = new ClientChain(aftermath === 'later_refusal' ? [first, second] : [first]);
      const method = { service: 'test.Echo', method: 'echo', pattern: RpcPattern.UNARY, idempotent: false };
      const call = (tag: string, signal?: AbortSignal) => chain.unary(peer!.transport, method, Buffer.from(tag), { signal, deadlineSecs: 2 });
      await expect(call('seed')).rejects.toMatchObject({code: StatusCode.UNAVAILABLE});
      if (aftermath === 'later_refusal') {
        await expect(call('refused')).rejects.toThrow('circuit breaker is open');
        expect(seen).toEqual(['seed']);
        await new Promise(r => setTimeout(r, 180));
      } else {
        const pending = call('hold', abort.signal);
        const terminal = pending.then(value => ({value}), error => ({error}));
        await until(() => seen.includes('hold'));
        expect(first.halfOpenInFlight).toBe(1);
        abort.abort();
        expect(await terminal).toMatchObject({error: {code: StatusCode.CANCELLED}});
        release();
      }
      expect(first.currentState).toBe('half_open');
      expect(first.halfOpenInFlight).toBe(0);
      expect(Buffer.from(await call('healthy-probe') as Uint8Array).toString()).toBe('healthy-probe');
      expect(first.currentState).toBe('closed');
      if (aftermath === 'later_refusal') expect(second.currentState).toBe('closed');
      await until(() => loop.native.liveCalls() === 0);
      expect(seen).toEqual(aftermath === 'later_refusal' ? ['seed', 'healthy-probe'] : ['seed', 'hold', 'healthy-probe']);
    } finally {
      abort.abort(); release();
      try { peer?.conn.close(0, 'done'); } catch { /* closed */ }
      await peer?.node.close();
      await loop.close();
    }
  });
});
