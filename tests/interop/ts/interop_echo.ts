#!/usr/bin/env bun
/**
 * Cross-binding interop smoke: the TypeScript side of `tests/interop/`.
 *
 * Contract lives in ./src (generated metadata: `aster-gen -p tsconfig.json`,
 * written to ./aster-rpc.generated.ts by tests/interop/run.py).
 *
 *   bun run interop_echo.ts contract
 *   bun run interop_echo.ts server            # ASTER_RELAY_MODE=disabled ASTER_BIND_ADDR=127.0.0.1:0
 *   bun run interop_echo.ts client <aster1…>
 *
 *   bun run interop_echo.ts expose-server     # expose mini-matrix: HTTP service `interop-http`
 *
 * Contract and semantics: tests/interop/rust/contract.rs. Output protocol (parsed
 * by tests/interop/run.py): `CONTRACT_ID <hex>`, `SESSION_CONTRACT_ID <hex>`,
 * `ADDRESS <aster1…>` (server), `CASE <name> OK|FAIL …` (client).
 */

import {
  AsterServer,
  IrohTransport,
  createClient,
  createXlangCodec,
  getXlangForyAndType,
  getServiceInfo,
  registerGenerated,
  contractIdFromContract,
  fromServiceInfo,
  setNativeContract,
  i32,
  withTraceContext,
  Node,
  RpcError,
  StatusCode,
} from '@aster-rpc/aster';
import { randomBytes } from 'node:crypto';
import { loadNative } from '@aster-rpc/aster/native.js';
import { SERVICES, WIRE_TYPES, BUILD_ALL_TYPES } from './aster-rpc.generated.ts';
import { InteropEcho, InteropSession } from './src/service.ts';
import { EchoRequest, EchoReply } from './src/types.ts';

const RPC_ALPN = Buffer.from('aster/1');
const EXPOSE_ALPN = 'aster-expose/interop/1';
const EXPOSE_SERVICE = 'interop-http';

/** The shared cases. Must match `cases()` in the Rust and Python drivers. */
function cases(clientLang: string): [string, EchoRequest][] {
  const big = new Uint8Array(20_000);
  for (let i = 0; i < big.length; i++) big[i] = i % 251;
  return [
    ['small', new EchoRequest({ message: `hello from ${clientLang}`, count: i32(41), data: new Uint8Array([0, 1, 2, 255]) })],
    // > 4096 bytes on the wire and highly compressible: exercises zstd.
    ['zstd', new EchoRequest({ message: 'héllo ✓ '.repeat(200), count: i32(-123_456), data: big })],
    // A single string field longer than 4095 bytes (Fory string-length edge).
    ['long_string', new EchoRequest({ message: 'x'.repeat(5000), count: i32(2 ** 31 - 2), data: new Uint8Array() })],
  ];
}

function check(req: EchoRequest, reply: EchoReply): string {
  if (reply.message !== req.message) {
    throw new Error(`message mismatch (len ${reply.message?.length} vs ${req.message.length})`);
  }
  const want = (req.count + 1) | 0;
  if (reply.count !== want) throw new Error(`count ${reply.count} != ${want}`);
  const got = reply.data instanceof Uint8Array ? reply.data : new Uint8Array(reply.data as any ?? []);
  if (Buffer.compare(Buffer.from(got), Buffer.from(req.data)) !== 0) {
    throw new Error(`data mismatch (len ${got.length} vs ${req.data.length})`);
  }
  if (!reply.server_lang) throw new Error('empty server_lang');
  return reply.server_lang;
}

const generated = { SERVICES, WIRE_TYPES, buildAllTypes: BUILD_ALL_TYPES } as any;

/** Register generated metadata + Fory types; returns the client codec. */
function registerAll() {
  const { fory, Type } = getXlangForyAndType();
  const codec = createXlangCodec(fory, Type);
  registerGenerated({ ...generated, codec, fory, Type });
  return codec;
}

function contractId(cls: any = InteropEcho): string {
  setNativeContract(loadNative() as any);
  return contractIdFromContract(fromServiceInfo(getServiceInfo(cls) as any));
}

async function waitStdinEof(): Promise<void> {
  process.stdin.resume();
  await new Promise<void>((res) => {
    process.stdin.on('end', res);
    process.stdin.on('close', res);
  });
}

async function serve(): Promise<void> {
  process.env.ASTER_RELAY_MODE ??= 'disabled';
  process.env.ASTER_BIND_ADDR ??= '127.0.0.1:0';
  const server = new AsterServer({
    services: [new InteropEcho(), new InteropSession()],
    allowAllConsumers: true,
    generated,
    config: { logLevel: 'warn' } as any,
  });
  await server.start();
  // Direct addresses appear shortly after bind.
  for (let i = 0; i < 100; i++) {
    if ((server as any)._node.directAddresses().length) break;
    await new Promise((r) => setTimeout(r, 50));
  }
  console.log(`ADDRESS ${server.address}`);
  void server.serve();
  await waitStdinEof();
  await server.close();
  process.exit(0);
}

async function runClient(ticket: string): Promise<boolean> {
  const native = loadNative();
  const codec = registerAll();
  const node = await native.Node.start({
    alpns: [RPC_ALPN],
    relayMode: 'disabled',
    bindAddr: '127.0.0.1:0',
  });
  let conn: any;
  try {
    const t = native.asterTicketFromString(ticket);
    // Dial with explicit direct addresses: a bare-id connect relies on
    // relay/discovery and hangs offline.
    conn = await withTimeout(node.connectNodeAddr(t.endpointId, RPC_ALPN, t.directAddrs, undefined), 15_000);
  } catch (e) {
    console.log(`CASE connect FAIL ${e}`);
    await node.close();
    return false;
  }
  const client = createClient(InteropEcho, new IrohTransport(conn, codec));
  let allOk = true;
  for (const [name, req] of cases('ts')) {
    try {
      const reply = (await withTimeout(client.echo(req), 15_000)) as EchoReply;
      console.log(`CASE ${name} OK server_lang=${check(req, reply)}`);
    } catch (e) {
      allOk = false;
      console.log(`CASE ${name} FAIL ${String(e).split('\n')[0]}`);
    }
  }
  const transport = new IrohTransport(conn, codec);
  for (const [name, run] of [
    ['server_stream', () => streamCase(client)],
    ['client_stream', () => ingestCase(client)],
    ['bidi', () => chatCase(client)],
    ['deadline', () => deadlineCase(client)],
    ['cancel', () => cancelCase(client)],
    ['session', () => sessionCase(transport)],
    ['trace', () => traceCase(client)],
  ] as [string, () => Promise<string>][]) {
    try {
      console.log(`CASE ${name} OK server_lang=${await withTimeout(run(), 30_000)}`);
    } catch (e) {
      allOk = false;
      console.log(`CASE ${name} FAIL ${String(e).split('\n')[0]}`);
    }
  }
  try { conn.close(0, 'done'); } catch { /* ignore */ }
  await node.close();
  return allOk;
}

/** `generate` with count 3. Must match the Rust and Python drivers. */
async function streamCase(client: any): Promise<string> {
  const req = new EchoRequest({ message: 'stream', count: i32(3), data: new Uint8Array([7, 8, 9]) });
  const replies: EchoReply[] = [];
  for await (const r of client.generate(req) as AsyncIterable<EchoReply>) replies.push(r);
  if (replies.length !== 3) throw new Error(`${replies.length} replies != 3`);
  replies.forEach((r, i) => {
    const data = Buffer.from(r.data instanceof Uint8Array ? r.data : new Uint8Array(r.data as any ?? []));
    if (r.count !== i || r.message !== req.message || Buffer.compare(data, Buffer.from(req.data)) !== 0) {
      throw new Error(`reply ${i} mismatch: ${JSON.stringify(r)}`);
    }
  });
  return replies[0].server_lang;
}

/** `ingest` with three requests: one reply, count 3, last message echoed. */
async function ingestCase(client: any): Promise<string> {
  async function* reqs() {
    for (let i = 0; i < 3; i++) {
      yield new EchoRequest({ message: `m${i}`, count: i32(i), data: new Uint8Array([i]) });
    }
  }
  const reply = (await client.ingest(reqs())) as EchoReply;
  if (reply.count !== 3 || reply.message !== 'm2') throw new Error(`reply mismatch: ${JSON.stringify(reply)}`);
  if (!reply.server_lang) throw new Error('empty server_lang');
  return reply.server_lang;
}

/** `chat`, interleaved: send c<i>, read its reply, three times; then end the requests and expect the stream to end. */
async function chatCase(client: any): Promise<string> {
  const ch = client.chat();
  const it = ch[Symbol.asyncIterator]();
  let lang = '';
  for (let i = 0; i < 3; i++) {
    const req = new EchoRequest({ message: `c${i}`, count: i32(100 + i), data: new Uint8Array([i, 9]) });
    await ch.send(req);
    const step = await it.next();
    if (step.done) throw new Error(`stream ended before reply ${i}`);
    const r = step.value as EchoReply;
    if (r.count !== i || r.message !== req.message || Buffer.compare(Buffer.from(r.data), Buffer.from(req.data)) !== 0) {
      throw new Error(`reply ${i} mismatch: ${JSON.stringify(r)}`);
    }
    lang = r.server_lang;
  }
  await ch.close();
  const end = await it.next();
  if (!end.done) throw new Error(`unexpected extra reply ${JSON.stringify(end.value)}`);
  if (!lang) throw new Error('empty server_lang');
  return lang;
}

const probe = () => new EchoRequest({ message: 'probe' });

async function cancelledCount(client: any): Promise<number> {
  return ((await withTimeout(client.cancelled(probe()), 10_000)) as EchoReply).count;
}

async function expectServerCancel(client: any, want: number): Promise<void> {
  let seen = 0;
  for (let i = 0; i < 50; i++) {
    seen = await cancelledCount(client);
    if (seen >= want) return;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`server never observed the cancellation (count ${seen}, want ${want})`);
}

const TRACE_ID = '4bf92f3577b34da6a3ce929d0e0e4736';

/** `traceEcho` under a known, sampled `traceparent`: the server must report the same trace id. */
async function traceCase(client: any): Promise<string> {
  const traceparent = `00-${TRACE_ID}-${randomBytes(8).toString('hex')}-01`;
  const reply = (await withTraceContext(traceparent, () =>
    withTimeout(client.traceEcho(probe()), 15_000),
  )) as EchoReply;
  if (reply.message !== TRACE_ID) throw new Error(`server saw trace id '${reply.message}', want ${TRACE_ID}`);
  return reply.server_lang;
}

/** `hold` with a 1 s deadline: DEADLINE_EXCEEDED, and the server observes a cancellation. */
async function deadlineCase(client: any): Promise<string> {
  const before = await cancelledCount(client);
  let r: unknown;
  try {
    r = await withTimeout(client.hold(new EchoRequest({ message: 'hold' }), { deadlineSecs: 1 }), 15_000);
  } catch (e) {
    if (!(e instanceof RpcError) || e.code !== StatusCode.DEADLINE_EXCEEDED) {
      throw new Error(`expected DEADLINE_EXCEEDED, got ${String(e)}`);
    }
    await expectServerCancel(client, before + 1);
    return 'ts';
  }
  throw new Error(`hold returned ${JSON.stringify(r)} instead of an error`);
}

/** `hold` cancelled by the client (AbortSignal) after 300 ms: the server observes it. */
async function cancelCase(client: any): Promise<string> {
  const before = await cancelledCount(client);
  const ac = new AbortController();
  setTimeout(() => ac.abort(), 300);
  let r: unknown;
  try {
    r = await withTimeout(client.hold(new EchoRequest({ message: 'hold' }), { signal: ac.signal }), 15_000);
  } catch (e) {
    if (!(e instanceof RpcError) || e.code !== StatusCode.CANCELLED) {
      throw new Error(`expected CANCELLED, got ${String(e)}`);
    }
    await expectServerCancel(client, before + 1);
    return 'ts';
  }
  throw new Error(`hold returned ${JSON.stringify(r)} before the client cancelled it`);
}

/** Two sessions on one connection: state persists per session, isolated between them. */
async function sessionCase(transport: IrohTransport): Promise<string> {
  const a: any = createClient(InteropSession, transport.openSession());
  const b: any = createClient(InteropSession, transport.openSession());
  const steps: [any, number, number, string][] = [
    [a, 5, 5, 'calls=1'], [a, 7, 12, 'calls=2'], [b, 1, 1, 'calls=1'], [a, 0, 12, 'calls=3'], [b, 10, 11, 'calls=2'],
  ];
  let lang = '';
  for (const [i, [s, add, total, calls]] of steps.entries()) {
    const r = (await s.add(new EchoRequest({ message: 'add', count: i32(add) }))) as EchoReply;
    if (r.count !== total || r.message !== calls) {
      throw new Error(`step ${i}: got total ${r.count} ${r.message}, want total ${total} ${calls}`);
    }
    lang = r.server_lang;
  }
  if (!lang) throw new Error('empty server_lang');
  return lang;
}

/** Expose mini-matrix server: `GET /hello`, `POST /echo` (request body streamed back). */
async function exposeServe(): Promise<void> {
  const node = await Node.start({ alpns: [EXPOSE_ALPN], relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as any);
  const expose = node.expose();
  const headers = { 'x-server-lang': 'ts' };
  expose.exposeHttp(EXPOSE_SERVICE, async (req: any) => {
    if (req.method === 'GET' && req.uri === '/hello') return { status: 200, headers, body: 'hello from ts' };
    if (req.method === 'POST' && req.uri === '/echo') {
      async function* echo() {
        if (Buffer.isBuffer(req.body)) yield req.body;
        else for await (const c of req.body) yield c;
      }
      return { status: 200, headers, body: echo() };
    }
    return { status: 404, headers, body: '' };
  });
  node.serveExpose(expose, [EXPOSE_ALPN]);
  for (let i = 0; i < 100 && !node.directAddresses().length; i++) await new Promise((r) => setTimeout(r, 50));
  const native = loadNative();
  const ticket = native.asterTicketToString({
    endpointId: node.id,
    relayAddr: undefined,
    directAddrs: node.directAddresses(),
  } as any);
  console.log(`ADDRESS ${ticket}`);
  await waitStdinEof();
  await node.close();
  process.exit(0);
}

function withTimeout<T>(p: Promise<T>, ms: number): Promise<T> {
  return Promise.race([
    p,
    new Promise<T>((_, rej) => setTimeout(() => rej(new Error(`timeout after ${ms}ms`)), ms)),
  ]);
}

// Server registers generated metadata itself (AsterServer.start); the
// contract id needs the ServiceInfo stamped either way.
const mode = process.argv[2];
if (mode !== 'server') registerAll();
else registerGenerated({ ...generated });
console.log(`CONTRACT_ID ${contractId()}`);
console.log(`SESSION_CONTRACT_ID ${contractId(InteropSession)}`);

if (mode === 'contract') {
  process.exit(0);
} else if (mode === 'server') {
  await serve();
} else if (mode === 'expose-server') {
  await exposeServe();
} else if (mode === 'client' && process.argv[3]) {
  process.exit((await runClient(process.argv[3])) ? 0 : 1);
} else {
  console.error('usage: interop_echo.ts contract | server | client <aster1-ticket> | expose-server');
  process.exit(2);
}
