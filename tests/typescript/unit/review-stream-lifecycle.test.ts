import { describe, it, expect, vi } from 'vitest';
import { IrohTransport } from '../../../bindings/typescript/packages/aster/src/transport/iroh.js';
import { withInboundTrace } from '../../../bindings/typescript/packages/aster/src/tracing.js';

const codec = {
  encode: (value: unknown) => Buffer.from(JSON.stringify(value)),
  decode: (value: Uint8Array) => JSON.parse(Buffer.from(value).toString()),
};

describe('review: streaming cancellation during native open', () => {
  it.each(['serverStream', 'clientStream'] as const)('%s observes an abort while opening', async (pattern) => {
    const controller = new AbortController();
    const call = {
      cancel: vi.fn(async () => {}),
      next: vi.fn(async () => null),
      response: vi.fn(async () => Buffer.alloc(0)),
      finish: vi.fn(async () => {}),
    };
    // Abort at the actual asynchronous boundary: before the native call handle
    // is returned, after the transport checked signal.aborted.
    const rpc = { [pattern]: async () => { controller.abort(); return call; } };
    const transport = new IrohTransport({}, codec, { rpc: rpc as any });
    try {
      if (pattern === 'serverStream') {
        for await (const _ of transport.serverStream('Echo', 'watch', {}, { signal: controller.signal })) {}
      } else {
        await transport.clientStream('Echo', 'ingest', (async function* () {})(), { signal: controller.signal });
      }
    } catch {
      // swallow-ok: rejecting on cancellation is valid; the native cancel is asserted next
    }
    expect(call.cancel).toHaveBeenCalledOnce();
  });
});

it('review: outbound RPC forwards baggage even without a traceparent', async () => {
  const unary = vi.fn(async (..._args: unknown[]) => Buffer.alloc(0));
  const transport = new IrohTransport({}, codec, { rpc: { unary } as any });
  await withInboundTrace(undefined, 'tenant=acme', () => transport.unary('Echo', 'echo', {}));
  expect(unary.mock.calls[0]?.[5]).toEqual(['baggage', 'tenant=acme']);
});


describe('client-stream local producer failures', () => {
  it.each(['source', 'encode'])('cancels native on a %s failure', async (failure) => {
    const error = new Error('local failure');
    const call = {
      cancel: vi.fn(async () => {}), send: vi.fn(async () => {}),
      sendMany: vi.fn(async () => {}), finish: vi.fn(async () => {}),
      response: () => new Promise<Buffer>(() => {}),
    };
    const transport = new IrohTransport({}, {
      ...codec, encode: () => { if (failure === 'encode') throw error; return Buffer.alloc(0); },
    }, { rpc: { clientStream: async () => call } as any });
    const requests = {
      [Symbol.asyncIterator]() { return this; },
      async next() { if (failure === 'source') throw error; return { done: false as const, value: 1 }; },
      return: vi.fn(() => Promise.reject(new Error('cleanup failed'))),
    };
    await expect(transport.clientStream('Echo', 'ingest', requests)).rejects.toBe(error);
    expect(call.cancel).toHaveBeenCalledOnce();
    expect(requests.return).toHaveBeenCalledOnce();
    expect(call.finish).not.toHaveBeenCalled();
  });
});


describe('client-stream terminal races', () => {
  it.each(['resolve', 'reject', 'sync-value'])('ignores a late producer %s after abort', async (late) => {
    let resolveNext!: (value: IteratorResult<unknown>) => void;
    let rejectNext!: (error: Error) => void;
    let entered!: () => void;
    const parked = new Promise<void>((resolve) => { entered = resolve; });
    const next = new Promise<IteratorResult<unknown>>((resolve, reject) => { resolveNext = resolve; rejectNext = reject; });
    let rejectResponse!: (error: Error) => void;
    const response = new Promise<Buffer>((_, reject) => { rejectResponse = reject; });
    const call = {
      send: vi.fn(async () => {}), sendMany: vi.fn(async () => {}), finish: vi.fn(async () => {}),
      response: () => response,
      cancel: vi.fn(async () => { rejectResponse(new Error('cancelled')); }),
    };
    const requests = {
      [Symbol.asyncIterator]() { return this; },
      next() { entered(); return next; },
      return: vi.fn(() => Promise.reject(new Error('cleanup failed'))),
    };
    const controller = new AbortController();
    const transport = new IrohTransport({}, codec, { rpc: { clientStream: async () => call } as any });
    const source = late === 'sync-value' ? {
      [Symbol.iterator]() { return this; },
      next() { entered(); return { done: false, value: next.then((item) => item.value) }; },
      return: requests.return,
    } : requests;
    const result = transport.clientStream('Echo', 'ingest', source as AsyncIterable<unknown>, { signal: controller.signal });
    const rejected = expect(result).rejects.toThrow();
    await parked;
    controller.abort();
    await rejected;
    if (late !== 'reject') resolveNext({ done: false, value: 'late' });
    else rejectNext(new Error('late source failure'));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(call.send).not.toHaveBeenCalled();
    expect(call.finish).not.toHaveBeenCalled();
    expect(call.cancel).toHaveBeenCalledOnce();
    expect(requests.return).toHaveBeenCalledOnce();
  });

  it.each(['async', 'sync', 'sync-promised'])('finishes an exhausted %s source without calling return', async (kind) => {
    let answer!: (value: Buffer) => void;
    const response = new Promise<Buffer>((resolve) => { answer = resolve; });
    const call = {
      send: vi.fn(async () => {}), sendMany: vi.fn(async () => {}), cancel: vi.fn(async () => {}),
      finish: vi.fn(async () => { answer(Buffer.from('42')); }), response: () => response,
    };
    let count = 0;
    const requests = {
      [Symbol.asyncIterator]() { return this; },
      async next() { return ++count === 1 ? { done: false as const, value: 1 } : { done: true as const, value: undefined }; },
      return: vi.fn(async () => ({ done: true as const, value: undefined })),
    };
    const transport = new IrohTransport({}, codec, { rpc: { clientStream: async () => call } as any });
    const source = kind === 'async' ? requests : {
      [Symbol.iterator]() { return this; },
      next() { return ++count === 1 ? { done: false as const, value: kind === 'sync-promised' ? Promise.resolve(1) : 1 } : { done: true as const, value: undefined }; },
      return: requests.return,
    };
    expect(await transport.clientStream('Echo', 'ingest', source as AsyncIterable<unknown>)).toBe(42);
    expect(call.send).toHaveBeenCalledExactlyOnceWith(Buffer.from('1'));
    expect(call.finish).toHaveBeenCalledOnce();
    expect(call.cancel).not.toHaveBeenCalled();
    expect(requests.return).not.toHaveBeenCalled();
  });
});
