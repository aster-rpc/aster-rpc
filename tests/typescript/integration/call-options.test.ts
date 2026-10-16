/**
 * Per-call options reach the server handler over QUIC: the deadline and the
 * metadata of every pattern, from the typed proxy, the dynamic
 * `ProxyClient` and an `aster-gen` generated client, through the
 * config-driven `AsterServer` / `AsterClientWrapper`.
 */
import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { pathToFileURL } from 'node:url';
import {
  AsterClientWrapper,
  AsterServer,
  BidiStream,
  CallContext,
  ClientStream,
  ProxyClient,
  Rpc,
  ServerStream,
  Service,
  WireType,
} from '@aster-rpc/aster';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.js';
import { generateTypescriptClients } from '../../../bindings/typescript/packages/aster/src/codegen/client.js';

const available = !!tryLoadNative();
const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;

@WireType('callopts/Probe')
class Probe {
  note = '';
  constructor(init?: Partial<Probe>) { if (init) Object.assign(this, init); }
}

/** What the handler saw: the `x-call` metadata value and the ms left of its deadline (-1 = none). */
@WireType('callopts/Seen')
class Seen {
  meta = '';
  remainingMs = 0;
  constructor(init?: Partial<Seen>) { if (init) Object.assign(this, init); }
}

function seen(ctx: CallContext | undefined): Seen {
  const r = ctx?.remainingSeconds;
  return new Seen({
    meta: ctx?.metadata['x-call'] ?? '',
    remainingMs: r === undefined ? -1 : Math.round(r * 1000),
  });
}

@Service({ name: 'CallOpts', version: 1 })
class CallOpts {
  @Rpc({ request: Probe, response: Seen })
  async u(_req: Probe, ctx: CallContext): Promise<Seen> { return seen(ctx); }

  @ServerStream({ request: Probe, response: Seen })
  async *ss(_req: Probe, ctx: CallContext): AsyncGenerator<Seen> { yield seen(ctx); }

  @ClientStream({ request: Probe, response: Seen })
  async cs(reqs: AsyncIterable<Probe>, ctx: CallContext): Promise<Seen> {
    for await (const _ of reqs) { /* drain */ }
    return seen(ctx);
  }

  @BidiStream({ request: Probe, response: Seen })
  async *bi(reqs: AsyncIterable<Probe>, ctx: CallContext): AsyncGenerator<Seen> {
    for await (const _ of reqs) yield seen(ctx);
  }
}

let typeInfos: any[] | undefined;
function buildAllTypes(_fory: any, Type: any, codec: any): Map<string, any> {
  if (!typeInfos) {
    const struct = (cls: any, typeName: string, props: Record<string, any>) => {
      const t = Type.struct({ namespace: 'callopts', typeName }, props, { withConstructor: true });
      t.initMeta(cls);
      return t;
    };
    typeInfos = [
      struct(Probe, 'Probe', { note: Type.string() }),
      struct(Seen, 'Seen', { meta: Type.string(), remainingMs: Type.int64() }),
    ];
  }
  for (const t of typeInfos) codec.registerType(t);
  return new Map();
}
const generated = { SERVICES: [] as any[], WIRE_TYPES: [] as any[], buildAllTypes };

/** Calls one pattern; returns what the handler saw. */
type Pattern = (opts: { deadlineSecs: number; metadata: Record<string, string> }) => Promise<Seen>;

async function* probes(): AsyncGenerator<Probe> { yield new Probe({ note: 'p' }); }

async function first<T>(it: AsyncIterable<T>): Promise<T> {
  for await (const x of it) return x;
  throw new Error('stream ended with no item');
}

async function bidiOnce(ch: any): Promise<Seen> {
  await ch.send(new Probe({ note: 'p' }));
  const iter = ch[Symbol.asyncIterator]();
  const { value } = await iter.next();
  await ch.close();
  return value as Seen;
}

describe.skipIf(!available)('per-call options reach the handler', () => {
  let server: AsterServer;
  let client: AsterClientWrapper;
  let genDir: string;
  let GeneratedClient: any;

  beforeAll(async () => {
    server = new AsterServer({ services: [new CallOpts()], config: { ...OFFLINE }, generated });
    await server.start();
    client = new AsterClientWrapper({ address: server.address, config: { ...OFFLINE }, generated });
    await client.connect();

    genDir = fs.mkdtempSync(path.join(os.tmpdir(), 'aster-callopts-gen-'));
    const methods = ['u', 'ss', 'cs', 'bi'].map((name, i) => ({
      name,
      pattern: ['unary', 'server_stream', 'client_stream', 'bidi_stream'][i],
      request_type: 'Probe',
      response_type: 'Seen',
    }));
    generateTypescriptClients(
      [{ service: 'CallOpts', version: 1, contract_id: '', scoped: 'shared', methods }],
      { outDir: genDir, namespace: 'gen' },
    );
    GeneratedClient = (await import(pathToFileURL(path.join(genDir, 'gen/services/call-opts-v1.ts')).href)).CallOptsClient;
  });

  afterAll(async () => {
    await client?.close();
    await server?.close();
    if (genDir) fs.rmSync(genDir, { recursive: true, force: true });
  });

  function patterns(c: any, kind: 'typed' | 'proxy' | 'generated'): Record<string, Pattern> {
    const req = () => new Probe({ note: 'p' });
    if (kind === 'proxy') {
      return {
        unary: (o) => c.u(req(), o),
        server_stream: (o) => first(c.ss.stream(req(), o)),
        client_stream: (o) => c.cs(probes(), o),
        bidi_stream: (o) => bidiOnce(c.bi.bidi(o)),
      };
    }
    return {
      unary: (o) => c.u(req(), o),
      server_stream: (o) => first(c.ss(req(), o)),
      client_stream: (o) => c.cs(probes(), o),
      bidi_stream: (o) => bidiOnce(c.bi(o)),
    };
  }

  async function clients() {
    const transport = (client as any).transport;
    return {
      typed: await client.client(CallOpts),
      proxy: new ProxyClient('CallOpts', transport, new Map([['u', Probe], ['ss', Probe], ['cs', Probe], ['bi', Probe]])),
      generated: await GeneratedClient.fromConnection({ listServices: async () => [{ name: 'CallOpts' }], transport }),
    };
  }

  for (const kind of ['typed', 'proxy', 'generated'] as const) {
    it(`${kind}: deadline and metadata on every pattern`, async () => {
      const c = (await clients())[kind];
      for (const [pattern, call] of Object.entries(patterns(c, kind))) {
        const got = await call({ deadlineSecs: 20, metadata: { 'x-call': `${kind}-${pattern}` } });
        expect(got.meta, `${kind} ${pattern}`).toBe(`${kind}-${pattern}`);
        expect(Number(got.remainingMs), `${kind} ${pattern}`).toBeGreaterThan(0);
        expect(Number(got.remainingMs), `${kind} ${pattern}`).toBeLessThanOrEqual(20_000);
      }
    });

    it(`${kind}: withOptions carries the options to every pattern`, async () => {
      const c = (await clients())[kind].withOptions({ deadlineSecs: 20, metadata: { 'x-call': `${kind}-with` } });
      for (const [pattern, call] of Object.entries(patterns(c, kind))) {
        const got = await call(undefined as any);
        expect(got.meta, `${kind} ${pattern}`).toBe(`${kind}-with`);
        expect(Number(got.remainingMs), `${kind} ${pattern}`).toBeGreaterThan(0);
      }
    });
  }

  it('no options: no metadata, and only the server-side default deadline', async () => {
    const typed: any = await client.client(CallOpts);
    const got: Seen = await typed.u(new Probe());
    expect(Number(got.remainingMs)).toBeGreaterThan(20_000);
    expect(got.meta).toBe('');
  });
});
