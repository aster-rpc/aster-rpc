/**
 * Graceful drain through the Rust drain (mirror item D) and the handler
 * `CallContext` additions (item H): principal vs peer, provenance,
 * `isCancelled()` / `cancelled()`, and `CallContext.current()` inside a
 * streaming handler's generator body. Config-driven `AsterServer` /
 * `AsterClientWrapper` over QUIC.
 */
import { describe, it, expect, afterEach } from 'vitest';
import {
  AsterClientWrapper,
  AsterServer,
  BidiStream,
  CallContext,
  RpcError,
  Rpc,
  ServerStream,
  Service,
  StatusCode,
  WireType,
} from '@aster-rpc/aster';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.js';

const available = !!tryLoadNative();
const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;

@WireType('drainctx/Msg')
class Msg {
  text = '';
  constructor(init?: Partial<Msg>) { if (init) Object.assign(this, init); }
}

function deferred<T = void>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => { resolve = r; });
  return { promise, resolve };
}

/** Per-test hooks the handlers call into. */
const hooks = {
  entered: deferred(),
  release: deferred(),
  streamCancelled: deferred<boolean>(),
};

@Service({ name: 'DrainCtx', version: 1 })
class DrainCtx {
  @Rpc({ request: Msg, response: Msg })
  async hold(req: Msg): Promise<Msg> {
    hooks.entered.resolve();
    await hooks.release.promise;
    return new Msg({ text: `held ${req.text}` });
  }

  @Rpc({ request: Msg, response: Msg })
  async who(_req: Msg, ctx: CallContext): Promise<Msg> {
    return new Msg({ text: JSON.stringify({ peer: ctx.peer, principal: ctx.principal, provenance: ctx.provenance, cancelled: ctx.isCancelled() }) });
  }

  @ServerStream({ request: Msg, response: Msg })
  async *wait(_req: Msg, ctx: CallContext): AsyncGenerator<Msg> {
    yield new Msg({ text: 'started' });
    await ctx.cancelled();
    hooks.streamCancelled.resolve(ctx.isCancelled() && ctx.signal.aborted);
  }

  @BidiStream({ request: Msg, response: Msg })
  async *echo(reqs: AsyncIterable<Msg>): AsyncGenerator<Msg> {
    // No explicit ctx parameter: the async-local context must reach here.
    for await (const r of reqs) yield new Msg({ text: `${r.text}:${CallContext.current()?.metadata['x-tag'] ?? 'none'}` });
  }
}

let typeInfos: any[] | undefined;
function buildAllTypes(_fory: any, Type: any, codec: any): Map<string, any> {
  if (!typeInfos) {
    const t = Type.struct({ namespace: 'drainctx', typeName: 'Msg' }, { text: Type.string() }, { withConstructor: true });
    t.initMeta(Msg);
    typeInfos = [t];
  }
  for (const t of typeInfos) codec.registerType(t);
  return new Map();
}
const generated = { SERVICES: [] as any[], WIRE_TYPES: [] as any[], buildAllTypes };

async function until(what: string, probe: () => boolean, ms = 10_000): Promise<void> {
  const end = Date.now() + ms;
  while (!probe()) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 10));
  }
}

async function pair() {
  const server = new AsterServer({ services: [new DrainCtx()], config: { ...OFFLINE }, generated });
  await server.start();
  const client = new AsterClientWrapper({ address: server.address, config: { ...OFFLINE }, generated });
  await client.connect();
  const svc: any = await client.client(DrainCtx);
  return { server, client, svc };
}

const cleanup: Array<() => Promise<void>> = [];
afterEach(async () => {
  hooks.release.resolve();
  for (const f of cleanup.splice(0).reverse()) await f().catch(() => {});
  hooks.entered = deferred();
  hooks.release = deferred();
  hooks.streamCancelled = deferred<boolean>();
});

describe.skipIf(!available)('AsterServer.drain', () => {
  it('lets an in-flight call finish, refuses new ones and reports it', async () => {
    const { server, client, svc } = await pair();
    cleanup.push(() => client.close(), () => server.close());

    const inFlight = svc.hold(new Msg({ text: 'a' }));
    await hooks.entered.promise;
    expect(server.isDraining).toBe(false);
    const draining = server.drain(10_000);
    await until('drain started', () => server.isDraining);

    // A new call on the existing connection is refused with the drain status.
    const refused = await svc.who(new Msg()).then(() => undefined, (e: unknown) => e);
    expect(refused).toBeInstanceOf(RpcError);
    expect((refused as RpcError).code).toBe(StatusCode.UNAVAILABLE);
    expect((refused as RpcError).isDraining).toBe(true);

    hooks.release.resolve();
    expect((await inFlight).text).toBe('held a');
    const report = await draining;
    expect(report).toMatchObject({
      callsInFlight: 1,
      callsCompleted: 1,
      callsCancelled: 0,
      deadlineReached: false,
    });
    expect(report.callsRefused).toBeGreaterThanOrEqual(1);
    expect(report.elapsedMs).toBeGreaterThan(0);
    // drain() closes the server afterwards.
    await expect(server.drain(0)).rejects.toThrow(/not serving/);
  });

  it('cancels what still runs at the deadline; the client sees isDraining', async () => {
    const { server, client, svc } = await pair();
    cleanup.push(() => client.close(), () => server.close());

    const inFlight = svc.hold(new Msg({ text: 'b' })).then(() => undefined, (e: unknown) => e);
    await hooks.entered.promise;
    const report = await server.drain(100);
    expect(report.callsInFlight).toBe(1);
    expect(report.callsCancelled).toBe(1);
    expect(report.deadlineReached).toBe(true);
    expect(report.connectionsClosed).toBeGreaterThanOrEqual(1);

    const failure = await inFlight;
    expect(failure).toBeInstanceOf(RpcError);
    expect((failure as RpcError).isDraining).toBe(true);
  });

  it('rejects a negative grace', async () => {
    const server = new AsterServer({ services: [new DrainCtx()], config: { ...OFFLINE }, generated });
    cleanup.push(() => server.close());
    await expect(server.drain(-1)).rejects.toThrow(RangeError);
  });
});

describe.skipIf(!available)('handler CallContext', () => {
  it('peer, principal and provenance over iroh', async () => {
    const { server, client, svc } = await pair();
    cleanup.push(() => client.close(), () => server.close());
    const seen = JSON.parse((await svc.who(new Msg())).text);
    expect(seen.peer).toBe(client.node!.id);
    // No authenticator: the principal is the transport peer.
    expect(seen.principal).toBe(seen.peer);
    expect(seen.provenance).toBe('iroh_authenticated');
    expect(seen.cancelled).toBe(false);
  });

  it('cancelled() resolves when the caller cancels a stream', async () => {
    const { server, client, svc } = await pair();
    cleanup.push(() => client.close(), () => server.close());
    const controller = new AbortController();
    const stream = svc.wait(new Msg(), { signal: controller.signal });
    const it = stream[Symbol.asyncIterator]();
    expect((await it.next()).value.text).toBe('started');
    controller.abort();
    await it.return?.().catch(() => {});
    expect(await hooks.streamCancelled.promise).toBe(true);
  });

  it('CallContext.current() inside a bidi generator body', async () => {
    const { server, client, svc } = await pair();
    cleanup.push(() => client.close(), () => server.close());
    const ch = svc.echo({ metadata: { 'x-tag': 't1' } });
    await ch.send(new Msg({ text: 'one' }));
    const iter = ch[Symbol.asyncIterator]();
    expect((await iter.next()).value.text).toBe('one:t1');
    await ch.close();
  });
});
