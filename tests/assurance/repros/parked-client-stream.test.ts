/** Real QUIC early rejection while the JS request iterator is parked. */
import { describe, expect, it } from 'vitest';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.ts';
import { IrohTransport, mapTransportError } from '../../../bindings/typescript/packages/aster/src/transport/iroh.ts';
import { StatusCode } from '../../../bindings/typescript/packages/aster/src/status.ts';

const native = tryLoadNative() as any;
if (!native?.RpcConnection) throw new Error('Parked client-stream diagnostic requires a native addon exposing RpcConnection');
const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0', logLevel: 'warn' };
const ALPN = Buffer.from('aster/1');
const SERVICE = {
  name: 'review.Parked', version: 1,
  methods: [{ name: 'upload', pattern: 'client_stream', request: { hash: 'a1'.repeat(32) }, response: { hash: 'b2'.repeat(32) } }],
};
const BYTES = { encode: (value: unknown) => value as Uint8Array, decode: (value: Uint8Array) => value };

async function bounded<T>(promise: Promise<T>, label: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label} hung after 5000ms`)), 5000);
    })]);
  } finally { clearTimeout(timer); }
}

describe('review: parked client-stream producer', () => {
  it('returns an early native rejection and closes the parked request iterator', async () => {
    const server = await native.DispatchServer.start(OFFLINE, [JSON.stringify(SERVICE)], { delivery: 'ring', builtinNodeInfo: false });
    let peer: any;
    let conn: any;
    let outcome: Promise<{ code: number }> | undefined;
    let release!: (result: IteratorResult<Buffer>) => void;
    const parkedNext = new Promise<IteratorResult<Buffer>>((resolve) => { release = resolve; });
    let entered!: () => void;
    const parked = new Promise<void>((resolve) => { entered = resolve; });
    let returns = 0;
    let nexts = 0;
    const requests = {
      [Symbol.asyncIterator]() { return this; },
      async next(): Promise<IteratorResult<Buffer>> {
        if (++nexts === 1) return { done: false, value: Buffer.from('first') };
        entered();
        return parkedNext;
      },
      async return(): Promise<IteratorResult<Buffer>> {
        returns++;
        release({ done: true, value: undefined });
        return { done: true, value: undefined };
      },
    };
    let observe!: (result: { code: number }) => void;
    const nativeResponse = new Promise<{ code: number }>((resolve) => { observe = resolve; });
    try {
      peer = await native.Node.start({ ...OFFLINE, alpns: [ALPN] });
      const ticket = native.asterTicketFromString(server.address);
      conn = await peer.connectNodeAddr(ticket.endpointId, ALPN, ticket.directAddrs, undefined);
      const rpc = new native.RpcConnection(conn);
      // Delegate every operation to the real native call. Observe only when
      // its response settles, independently of the outer transport Promise.
      const observedRpc = { clientStream: async (...args: any[]) => {
        const call = await rpc.clientStream(...args);
        return {
          send: (...args: any[]) => call.send(...args),
          sendMany: (...args: any[]) => call.sendMany(...args),
          finish: () => call.finish(), cancel: () => call.cancel(),
          response: () => {
            const response = call.response();
            void response.then(() => observe({ code: StatusCode.OK }), (error: unknown) => observe({ code: mapTransportError(error).code }));
            return response;
          },
        };
      } };
      const transport = new IrohTransport(conn, BYTES as any, { rpc: observedRpc as any });
      outcome = transport.clientStream('review.Parked', 'upload', requests).then(
        () => ({ code: StatusCode.OK }), (error) => ({ code: mapTransportError(error).code }),
      );
      const events = await bounded(server.nextBatch(1), 'native call admission');
      expect(events[0].request.toString()).toBe('first');
      await bounded(parked, 'request producer entry');
      expect(server.respond(events[0].callId, Buffer.alloc(0), StatusCode.INVALID_ARGUMENT, 'early rejection')).toBe(0);
      expect(await bounded(nativeResponse, 'native response')).toEqual({ code: StatusCode.INVALID_ARGUMENT });
      console.log(JSON.stringify({ phase: 'parked_producer_native_response', nativeCode: StatusCode.INVALID_ARGUMENT, liveCalls: server.liveCalls(), iteratorReturns: returns }));
      expect(await bounded(outcome, 'JS clientStream response after native rejection')).toEqual({ code: StatusCode.INVALID_ARGUMENT });
      expect(returns).toBe(1);
    } finally {
      // Always release the cooperative source before awaiting wrapper teardown.
      release({ done: true, value: undefined });
      try { if (outcome) await bounded(outcome, 'released wrapper teardown'); }
      finally {
        try { conn?.close(0, 'done'); } catch { /* ignore */ }
        await peer?.close();
        await server.close();
      }
    }
  });
});
