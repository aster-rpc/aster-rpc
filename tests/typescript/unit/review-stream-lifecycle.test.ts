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
      // Rejecting cancellation is valid; the native call must also be cancelled.
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
