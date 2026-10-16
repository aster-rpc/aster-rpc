/**
 * Per-call options (deadline, metadata, session id, ...) on every client
 * kind and every pattern: the typed proxy (`createClient`), the dynamic
 * `ProxyClient`, and `aster-gen` generated clients. Mirrors Rust's
 * `XClient::with_options(CallOptions { .. })`. A recording transport shows
 * what each call hands to the transport, which writes it into the stream
 * header (see `call-options.test.ts` under integration for the server side).
 */
import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { pathToFileURL } from 'node:url';
import {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  ProxyClient,
  createClient,
  timeouts,
  type AsterTransport,
  type BidiChannel,
  type CallOptions,
} from '@aster-rpc/aster';
import { generateTypescriptClients } from '../../../bindings/typescript/packages/aster/src/codegen/client.js';

interface Sent { pattern: string; method: string; opts?: CallOptions }

function recordingTransport(): { transport: AsterTransport; sent: Sent[] } {
  const sent: Sent[] = [];
  const transport: AsterTransport = {
    async unary(_s, method, _req, opts) { sent.push({ pattern: 'unary', method, opts }); return {}; },
    serverStream(_s, method, _req, opts) {
      sent.push({ pattern: 'server_stream', method, opts });
      return (async function* () { /* empty */ })();
    },
    async clientStream(_s, method, reqs, opts) {
      sent.push({ pattern: 'client_stream', method, opts });
      for await (const _ of reqs) { /* drain */ }
      return {};
    },
    bidiStream(_s, method, opts) {
      sent.push({ pattern: 'bidi_stream', method, opts });
      return {
        async send() {}, async close() {},
        async *[Symbol.asyncIterator]() { /* empty */ },
      } as unknown as BidiChannel;
    },
    async close() {},
  };
  return { transport, sent };
}

async function* one(): AsyncGenerator<{ v: number }> { yield { v: 1 }; }

@Service({ name: 'Opts', version: 3 })
class OptsService {
  @Rpc() async u(_r: { v: string }): Promise<object> { return {}; }
  @ServerStream() async *ss(_r: { v: string }): AsyncGenerator<object> { /* */ }
  @ClientStream() async cs(_r: AsyncIterable<object>): Promise<object> { return {}; }
  @BidiStream() async *bi(_r: AsyncIterable<object>): AsyncGenerator<object> { /* */ }
}

const OPTS: CallOptions = { deadlineSecs: 2.5, metadata: { 'x-call': 'c1' }, sessionId: 7 };

/** Every pattern got `deadlineSecs` 2.5, the metadata and the version. */
function expectAll(sent: Sent[], version: number, extraMeta: Record<string, string> = {}) {
  expect(sent.map((s) => s.pattern)).toEqual(['unary', 'server_stream', 'client_stream', 'bidi_stream']);
  for (const s of sent) {
    expect(s.opts?.deadlineSecs, s.pattern).toBe(2.5);
    expect(s.opts?.metadata, s.pattern).toEqual({ 'x-call': 'c1', ...extraMeta });
    expect(s.opts?.version, s.pattern).toBe(version);
  }
}

describe('typed proxy (createClient)', () => {
  it('passes per-call options on all four patterns, including a bidi call with only options', async () => {
    const { transport, sent } = recordingTransport();
    const c = createClient(OptsService, transport) as any;
    await c.u({ v: 'a' }, OPTS);
    for await (const _ of c.ss({ v: 'a' }, OPTS)) { /* */ }
    await c.cs(one(), OPTS);
    c.bi(OPTS);
    expectAll(sent, 3);
    expect(sent.every((s) => s.opts?.sessionId === 7)).toBe(true);
  });

  it('withOptions applies to every call, a call\'s own options merge over it', async () => {
    const { transport, sent } = recordingTransport();
    const base = createClient(OptsService, transport, { metadata: { 'x-client': 'k' } }) as any;
    const c = base.withOptions({ deadlineSecs: 2.5, metadata: { 'x-call': 'base' } });
    await c.u({ v: 'a' }, { metadata: { 'x-call': 'c1' } });
    for await (const _ of c.ss({ v: 'a' }, { metadata: { 'x-call': 'c1' } })) { /* */ }
    await c.cs(one(), { metadata: { 'x-call': 'c1' } });
    c.bi({ metadata: { 'x-call': 'c1' } });
    expectAll(sent, 3, { 'x-client': 'k' });
    // The original client is unchanged.
    await base.u({ v: 'a' });
    expect(sent[4]!.opts?.deadlineSecs).toBeUndefined();
  });

  it('keeps sub-second client-wide timeouts and timeouts() fractions', async () => {
    const { transport, sent } = recordingTransport();
    const c = createClient(OptsService, transport, { timeout: 0.25 }) as any;
    await c.u({ v: 'a' });
    await c.u({ v: 'a' }, timeouts(0.5));
    expect(sent.map((s) => s.opts?.deadlineSecs)).toEqual([0.25, 0.5]);
  });

  it('does not mistake a request message for options', async () => {
    class Req { deadlineSecs = 9; }
    const { transport, sent } = recordingTransport();
    const c = createClient(OptsService, transport) as any;
    await c.u(new Req());
    expect(sent[0]!.opts?.deadlineSecs).toBeUndefined();
  });
});

describe('ProxyClient', () => {
  it('passes per-call options on all four patterns', async () => {
    const { transport, sent } = recordingTransport();
    const p = new ProxyClient('Opts', transport, undefined, 3) as any;
    await p.u({ v: 'a' }, OPTS);
    for await (const _ of p.ss.stream({ v: 'a' }, OPTS)) { /* */ }
    await p.cs(one(), OPTS);
    await p.bi.bidi(OPTS).open();
    expectAll(sent, 3);
  });

  it('withOptions applies to every pattern', async () => {
    const { transport, sent } = recordingTransport();
    const p = (new ProxyClient('Opts', transport, undefined, 3)).withOptions(OPTS) as any;
    await p.u({ v: 'a' });
    for await (const _ of p.ss.stream({ v: 'a' })) { /* */ }
    await p.cs(one());
    await p.bi.bidi().open();
    expectAll(sent, 3);
  });
});

describe('generated clients', () => {
  const manifest = {
    service: 'Opts',
    version: 3,
    contract_id: 'cid',
    scoped: 'shared',
    methods: [
      { name: 'u', pattern: 'unary', request_type: 'Req', response_type: 'Resp' },
      { name: 'ss', pattern: 'server_stream', request_type: 'Req', response_type: 'Resp' },
      { name: 'cs', pattern: 'client_stream', request_type: 'Req', response_type: 'Resp' },
      { name: 'bi', pattern: 'bidi_stream', request_type: 'Req', response_type: 'Resp' },
      {
        name: 'inl', pattern: 'unary', request_type: 'Req', response_type: 'Resp', request_style: 'inline',
        inline_params: [{ name: 'v', kind: 'string' }],
      },
    ],
  };
  let out: string;
  let OptsClient: any;

  beforeAll(async () => {
    out = fs.mkdtempSync(path.join(os.tmpdir(), 'aster-call-options-'));
    generateTypescriptClients([manifest], { outDir: out, namespace: 'Ns', generatedAt: '2026-01-01T00:00:00Z' });
    const mod = await import(pathToFileURL(path.join(out, 'ns/services/opts-v3.ts')).href);
    OptsClient = mod.OptsClient;
  });
  afterAll(() => fs.rmSync(out, { recursive: true, force: true }));

  async function client(transport: AsterTransport) {
    return OptsClient.fromConnection({ listServices: async () => [{ name: 'Opts' }], transport });
  }

  it('emits a trailing callOptions on every method and withOptions', () => {
    const src = fs.readFileSync(path.join(out, 'ns/services/opts-v3.ts'), 'utf8');
    expect(src).toContain('type CallOptions } from "@aster-rpc/aster";');
    expect(src).toContain('async u(request: Req, callOptions?: CallOptions): Promise<Resp> {');
    expect(src).toContain('async *ss(request: Req, callOptions?: CallOptions): AsyncGenerator<Resp> {');
    expect(src).toContain('async cs(requests: AsyncIterable<Req>, callOptions?: CallOptions): Promise<Resp> {');
    expect(src).toContain('bi(callOptions?: CallOptions): BidiChannel {');
    expect(src).toContain('async inl(v: string, callOptions?: CallOptions): Promise<Resp> {');
    expect(src).toContain('withOptions(options: CallOptions): OptsClient {');
  });

  it('passes per-call options on all four patterns', async () => {
    const { transport, sent } = recordingTransport();
    const c = await client(transport);
    await c.u({}, OPTS);
    for await (const _ of c.ss({}, OPTS)) { /* */ }
    await c.cs(one(), OPTS);
    c.bi(OPTS);
    expectAll(sent, 3);
  });

  it('withOptions applies to every pattern; per-call metadata merges over it', async () => {
    const { transport, sent } = recordingTransport();
    const base = await client(transport);
    const c = base.withOptions({ deadlineSecs: 2.5, metadata: { 'x-base': 'b', 'x-call': 'base' } });
    await c.u({}, { metadata: { 'x-call': 'c1' } });
    for await (const _ of c.ss({}, { metadata: { 'x-call': 'c1' } })) { /* */ }
    await c.cs(one(), { metadata: { 'x-call': 'c1' } });
    c.bi({ metadata: { 'x-call': 'c1' } });
    expectAll(sent, 3, { 'x-base': 'b' });
    await c.inl('x');
    expect(sent[4]!.opts?.deadlineSecs).toBe(2.5);
    await base.u({});
    expect(sent[5]!.opts?.deadlineSecs).toBeUndefined();
  });
});
