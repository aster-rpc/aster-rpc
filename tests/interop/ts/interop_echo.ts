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
 * Output protocol (parsed by tests/interop/run.py): `CONTRACT_ID <hex>`,
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
} from '@aster-rpc/aster';
import { loadNative } from '@aster-rpc/aster/native.js';
import { SERVICES, WIRE_TYPES, BUILD_ALL_TYPES } from './aster-rpc.generated.ts';
import { InteropEcho } from './src/service.ts';
import { EchoRequest, EchoReply } from './src/types.ts';

const RPC_ALPN = Buffer.from('aster/1');

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

function contractId(): string {
  setNativeContract(loadNative() as any);
  return contractIdFromContract(fromServiceInfo(getServiceInfo(InteropEcho) as any));
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
    services: [new InteropEcho()],
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
  for (const [name, run] of [
    ['server_stream', () => streamCase(client)],
    ['client_stream', () => ingestCase(client)],
  ] as [string, () => Promise<string>][]) {
    try {
      console.log(`CASE ${name} OK server_lang=${await withTimeout(run(), 15_000)}`);
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

if (mode === 'contract') {
  process.exit(0);
} else if (mode === 'server') {
  await serve();
} else if (mode === 'client' && process.argv[3]) {
  process.exit((await runClient(process.argv[3])) ? 0 : 1);
} else {
  console.error('usage: interop_echo.ts contract | server | client <aster1-ticket>');
  process.exit(2);
}
