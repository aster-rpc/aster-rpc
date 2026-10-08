/** Wrapper ordering barriers delegate to real native calls over loopback QUIC. */
import { describe, expect, it } from 'vitest';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.ts';
import { IrohTransport } from '../../../bindings/typescript/packages/aster/src/transport/iroh.ts';
import { StatusCode } from '../../../bindings/typescript/packages/aster/src/status.ts';

const native = tryLoadNative() as any;
if (!native?.RpcConnection) throw new Error('Pool lifecycle tests require the native RPC addon');
const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0', logLevel: 'warn' };
const ALPN = Buffer.from('aster/1');
const SERVICE = {
  name: 'review.Pool', version: 1,
  methods: ['unary', 'bidi', 'server_stream', 'client_stream'].map((pattern) => ({
    name: pattern, pattern: pattern === 'bidi' ? 'bidi_stream' : pattern, request: { hash: 'a1'.repeat(32) }, response: { hash: 'b2'.repeat(32) },
  })),
};
const BYTES = { encode: (value: unknown) => value as Uint8Array, decode: (value: Uint8Array) => value };
function gate() {
  let release!: () => void;
  const promise = new Promise<void>((resolve) => { release = resolve; });
  return { promise, release };
}
async function bounded<T>(promise: Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error('RPC lifecycle watchdog expired')), 5000);
    })]);
  } finally { clearTimeout(timer); }
}
async function fixture() {
  const server = await native.DispatchServer.start(OFFLINE, [JSON.stringify(SERVICE)], { delivery: 'ring', builtinNodeInfo: false });
  const peer = await native.Node.start({ ...OFFLINE, alpns: [ALPN] });
  const ticket = native.asterTicketFromString(server.address);
  const conn = await peer.connectNodeAddr(ticket.endpointId, ALPN, ticket.directAddrs, undefined);
  const rpc = new native.RpcConnection(conn);
  return { server, rpc, conn, async close() { conn.close(0, 'done'); await peer.close(); await server.close(); } };
}

describe('transport: native pool and concurrent open recovery', () => {
  it.each(['serverStream', 'clientStream', 'bidi'] as const)('cancels a real %s handle returned after abort', async (pattern) => {
    const f = await fixture();
    const entered = gate(), release = gate();
    const controller = new AbortController();
    let cancels = 0;
    const rpc = { [pattern]: async (...args: any[]) => {
      const call = await f.rpc[pattern](...args);
      entered.release();
      await release.promise;
      // Count cancellation at the wrapper/native boundary, delegate actual I/O.
      return {
        cancel: () => { cancels++; return call.cancel(); },
        next: () => call.next(), response: () => call.response(),
        send: (value: Uint8Array) => call.send(value),
        sendMany: (values: Uint8Array[], last: boolean) => call.sendMany(values, last),
        finish: () => call.finish(),
      };
    } };
    const transport = new IrohTransport(f.conn, BYTES as any, { rpc: rpc as any });
    try {
      const options = { signal: controller.signal };
      const operation = pattern === 'serverStream'
        ? transport.serverStream('review.Pool', 'server_stream', Buffer.from('request'), options)[Symbol.asyncIterator]().next()
        : pattern === 'clientStream'
          ? transport.clientStream('review.Pool', 'client_stream', (async function* () { yield Buffer.from('request'); })(), options)
          : transport.bidiStream('review.Pool', 'bidi', options)[Symbol.asyncIterator]().next();
      const outcome = operation.then(() => StatusCode.OK, (error) => error.code);
      await bounded(entered.promise);
      controller.abort();
      release.release();
      expect(await bounded(outcome)).toBe(StatusCode.CANCELLED);
      expect(cancels).toBe(1);
      await bounded((async () => {
        while (f.server.liveCalls() !== 0) await new Promise((resolve) => setTimeout(resolve, 5));
      })());
      // A fresh connection-wide pooled call must still complete after cleanup.
      const healthy = new IrohTransport(f.conn, BYTES as any, { rpc: f.rpc });
      const result = healthy.unary('review.Pool', 'unary', Buffer.from('healthy'));
      let fresh: any;
      do {
        [fresh] = await bounded<any[]>(f.server.nextBatch(1));
      } while (fresh.request?.toString() !== 'healthy');
      expect(f.server.respond(fresh.callId, fresh.request)).toBe(0);
      expect(await bounded(result)).toEqual(Buffer.from('healthy'));
    } finally { release.release(); controller.abort(); await f.close(); }
  });

  it('concurrent bidi send and receive share one held native open', async () => {
    const f = await fixture();
    const entered = gate(), release = gate();
    let opens = 0;
    const rpc = { bidi: async (...args: any[]) => {
      opens++;
      const call = await f.rpc.bidi(...args);
      entered.release();
      await release.promise;
      return call;
    } };
    const transport = new IrohTransport(f.conn, BYTES as any, { rpc: rpc as any });
    try {
      const channel = transport.bidiStream('review.Pool', 'bidi');
      const sent = channel.send(Buffer.from('one'));
      const iterator = channel[Symbol.asyncIterator]();
      const received = iterator.next();
      await bounded(entered.promise);
      expect(opens).toBe(1);
      release.release();
      await bounded(sent);
      const [event] = await bounded<any[]>(f.server.nextBatch(1));
      expect(event.request.toString()).toBe('one');
      expect(f.server.send(event.callId, Buffer.from('answer'))).toBe(0);
      expect((await bounded(received)).value).toEqual(Buffer.from('answer'));
      await bounded(channel.close());
      await bounded(channel.close());
      expect(await bounded(f.server.recv(event.callId))).toBeNull();
      expect(f.server.finish(event.callId)).toBe(0);
      expect(await bounded(iterator.next())).toEqual({ done: true, value: undefined });
      expect(opens).toBe(1);
      expect(f.server.liveCalls()).toBe(0);
    } finally { release.release(); await f.close(); }
  });

  it('cancelled queued unary acquisitions leave a size-one pool reusable', async () => {
    const f = await fixture();
    f.rpc.setSharedPoolSize(1);
    const transport = new IrohTransport(f.conn, BYTES as any, { rpc: f.rpc });
    const controllers = Array.from({ length: 4 }, () => new AbortController());
    try {
      const active = transport.unary('review.Pool', 'unary', Buffer.from('active'));
      const [event] = await bounded<any[]>(f.server.nextBatch(1));
      expect(event.request.toString()).toBe('active');
      const queued = controllers.map((controller) => transport.unary('review.Pool', 'unary', Buffer.from('queued'), { signal: controller.signal }).then(
        () => StatusCode.OK, (error) => error.code,
      ));
      controllers.forEach((controller) => controller.abort());
      expect(await bounded(Promise.all(queued))).toEqual(Array(4).fill(StatusCode.CANCELLED));
      expect(f.server.liveCalls()).toBe(1);
      expect(f.server.respond(event.callId, Buffer.from('active-done'))).toBe(0);
      expect(await bounded(active)).toEqual(Buffer.from('active-done'));
      for (let i = 0; i < 3; i++) {
        const call = transport.unary('review.Pool', 'unary', Buffer.from(`fresh-${i}`));
        const [fresh] = await bounded<any[]>(f.server.nextBatch(1));
        expect(fresh.request.toString()).toBe(`fresh-${i}`);
        expect(f.server.respond(fresh.callId, fresh.request)).toBe(0);
        expect(await bounded(call)).toEqual(Buffer.from(`fresh-${i}`));
      }
      expect(f.server.liveCalls()).toBe(0);
      expect(f.rpc.sharedPoolSize).toBe(1);
    } finally { controllers.forEach((controller) => controller.abort()); await f.close(); }
  });
});
