/** Native local frame rejection must not wait for an unsent request's response. */
import { expect, it } from 'vitest';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.ts';
import {
  IrohTransport,
  mapTransportError,
} from '../../../bindings/typescript/packages/aster/src/transport/iroh.ts';
const native = tryLoadNative() as any;
if (!native?.RpcConnection) throw new Error('local validation control requires native RPC');
const offline = { relayMode: 'disabled', bindAddr: '127.0.0.1:0', logLevel: 'warn' };
const service = {
  name: 'review.LocalFrame',
  version: 1,
  methods: [{
    name: 'upload',
    pattern: 'client_stream',
    request: { hash: 'a1'.repeat(32) },
    response: { hash: 'b2'.repeat(32) },
  }],
};
async function bounded<T>(p: Promise<T>, label: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  // Diagnostic watchdog after an observed local error; no RPC deadline is set.
  try {
    return await Promise.race([
      p,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error(`${label} still pending after 500ms`)), 500);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
it.each(['empty', 'oversized'])('native %s encoded request rejects locally, cancels reader, and recovers without deadline', async (kind) => {
  const server = await native.DispatchServer.start(offline, [JSON.stringify(service)], {
    delivery: 'ring',
    builtinNodeInfo: false,
  });
  let peer: any, conn: any, call: any;
  let responseSettled = false;
  let local!: (error: unknown) => void;
  const localError = new Promise<unknown>((resolve) => { local = resolve; });
  let outcome: Promise<unknown> | undefined;
  try {
    peer = await native.Node.start({ ...offline, alpns: [Buffer.from('aster/1')] });
    const ticket = native.asterTicketFromString(server.address);
    conn = await peer.connectNodeAddr(ticket.endpointId, Buffer.from('aster/1'), ticket.directAddrs, undefined);
    const rpc = new native.RpcConnection(conn);
    const observed = {
      clientStream: async (...args: any[]) => {
      call = await rpc.clientStream(...args);
      return {
        send: (payload: Uint8Array) => call.send(payload).catch((error: unknown) => {
          local(error);
          throw error;
        }),
        sendMany: (...args: any[]) => call.sendMany(...args),
        finish: () => call.finish(),
        cancel: () => call.cancel(),
        response: () => {
          const p = call.response();
          void p.then(
            () => { responseSettled = true; },
            () => { responseSettled = true; },
          );
          return p;
        },
      };
      },
    };
    const payload = new Uint8Array(kind === 'empty' ? 0 : 16 * 1024 * 1024);
    let state = 0x12345678;
    for (let i = 0; i < payload.length; i++) {
      state ^= state << 13;
      state ^= state >>> 17;
      state ^= state << 5;
      payload[i] = state;
    }
    const codec = { encode: () => payload, decode: (p: Uint8Array) => p };
    const transport = new IrohTransport(conn, codec as any, { rpc: observed as any });
    outcome = transport.clientStream('review.LocalFrame', 'upload', [1]).then(
      () => { throw new Error(`invalid ${kind} request succeeded`); },
      (error) => error,
    );
    const rejected = await bounded(localError, 'native local rejection');
    console.log(JSON.stringify({
      phase: 'native_local_rejection',
      kind,
      error: String(rejected),
      responseSettled,
      liveCalls: server.liveCalls(),
    }));
    const error = await bounded(outcome, 'wrapper after native local rejection');
    expect(mapTransportError(error).message).toBe(mapTransportError(rejected).message);
    expect(server.liveCalls()).toBe(0);
    const healthy = new IrohTransport(conn, {
      encode: (p: unknown) => p as Uint8Array,
      decode: (p: Uint8Array) => p,
    } as any, { rpc });
    const result = healthy.clientStream('review.LocalFrame', 'upload', [Buffer.from('valid')]);
    const events = await server.nextBatch(1);
    expect(events[0].request.toString()).toBe('valid');
    expect(server.respond(events[0].callId, Buffer.from('after'), 0, '')).toBe(0);
    expect(Buffer.from(await result as Uint8Array).toString()).toBe('after');
    expect(server.liveCalls()).toBe(0);
  } finally {
    await call?.cancel();
    if (outcome) await bounded(outcome, 'owned cancelled wrapper cleanup');
    try { conn?.close(0, 'done'); } catch { }
    await peer?.close();
    await server.close();
  }
}, 10_000);
