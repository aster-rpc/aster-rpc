#!/usr/bin/env node
/**
 * T4.0 benchmark server: the `InteropEcho` contract (`tests/interop/`) served
 * by the native `DispatchServer` prototype, with JS handlers, through one of
 * the four event-delivery variants (dynamic-dispatch-design.md §6):
 *
 *   node ts_dispatch_server.mjs <tsfn|tsfn-batched|ring-await|ring-poll>
 *   bun  ts_dispatch_server.mjs <...>
 *
 * Plain ESM over the built package (`bindings/typescript/packages/aster/dist`,
 * `bun run build` there) so the same file runs on Node and Bun. Prints
 * `ADDRESS <aster1…>`, serves until stdin closes. Orchestrated by
 * `t4_bench.py`. Loopback only, relay off.
 *
 * Handlers do what the current TS server does per call: Fory-decode the
 * request into a class instance, run an `async` method (an async generator for
 * `generate`, an async iterable for `ingest`), Fory-encode the reply. The
 * contract is registered as a `DynamicService` JSON document (design §1.5).
 */

import { createRequire } from 'node:module';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const PKG = resolve(HERE, '../../bindings/typescript/packages/aster');
const require = createRequire(resolve(PKG, 'package.json'));
const { default: Fory, Type } = require('@apache-fory/core');
const { DispatchLoop, DispatchEventKind, DispatchFlags, respondCall, sendFrame, finishCall } =
  await import(resolve(PKG, 'dist/dispatch-server.js'));

const delivery = process.argv[2] ?? 'tsfn-batched';

// ── payload types (same Fory options + struct shapes as aster-rpc.generated.ts) ──

class EchoRequest {
  constructor(init) {
    this.message = '';
    this.count = 0;
    this.data = new Uint8Array();
    if (init) Object.assign(this, init);
  }
}
class EchoReply {
  constructor(init) {
    this.message = '';
    this.count = 0;
    this.data = new Uint8Array();
    this.server_lang = '';
    if (init) Object.assign(this, init);
  }
}

const fory = new Fory({ ref: false, compatible: true });
for (const [cls, name, fields] of [
  [EchoReply, 'EchoReply', { message: Type.string(), count: Type.int32(), data: Type.binary(), server_lang: Type.string() }],
  [EchoRequest, 'EchoRequest', { message: Type.string(), count: Type.int32(), data: Type.binary() }],
]) {
  const ts = Type.struct({ namespace: 'interop', typeName: name }, fields, { withConstructor: true });
  ts.initMeta(cls);
  fory.register(cls);
}
const decode = (b) => fory.deserialize(b);
const encode = (o) => fory.serialize(o);

// ── the service ──────────────────────────────────────────────────────────────

const REQ = { hash: '088c3c11e89089dd6f6578131e6ad72e61ec858cb540f5d09f5f21f31641dd3b', display: 'interop/EchoRequest' };
const REP = { hash: '4cd26d33fe1df34c37a3c657c52bff04a8d450bc5f7e26716a628548c1a50ab9', display: 'interop/EchoReply' };
export const INTEROP_ECHO = {
  name: 'InteropEcho',
  version: 1,
  methods: [
    { name: 'echo', pattern: 'unary', request: REQ, response: REP },
    { name: 'generate', pattern: 'server_stream', request: REQ, response: REP },
    { name: 'ingest', pattern: 'client_stream', request: REQ, response: REP },
  ],
};

const impl = {
  async echo(req) {
    return new EchoReply({ message: req.message, count: (req.count + 1) | 0, data: req.data, server_lang: 'ts' });
  },
  async *generate(req) {
    for (let i = 0; i < Math.max(req.count, 0); i++) {
      yield new EchoReply({ message: req.message, count: i, data: req.data, server_lang: 'ts' });
    }
  },
  async ingest(reqs) {
    let n = 0;
    let last = '';
    for await (const r of reqs) {
      n++;
      last = r.message;
    }
    return new EchoReply({ message: last, count: n | 0, data: new Uint8Array(), server_lang: 'ts' });
  },
};

const INTERNAL = 13;
const stats = { calls: 0, done: 0 };

async function serveCall(ev, srv) {
  const id = ev.callId;
  try {
    switch (ev.methodIdx) {
      case 0: {
        const reply = await impl.echo(decode(ev.request));
        await respondCall(srv, id, encode(reply));
        return;
      }
      case 1: {
        for await (const r of impl.generate(decode(ev.request))) {
          if ((await sendFrame(srv, id, encode(r))) !== 0) return; // cancelled / gone
        }
        await finishCall(srv, id);
        return;
      }
      case 2: {
        async function* requests() {
          if (ev.request && ev.request.length) yield decode(ev.request);
          if (ev.flags & DispatchFlags.FirstIsEnd) return;
          for (;;) {
            const b = await srv.recv(id);
            if (b === null) return;
            yield decode(b);
          }
        }
        const reply = await impl.ingest(requests());
        await respondCall(srv, id, encode(reply));
        return;
      }
      default:
        srv.respond(id, new Uint8Array(), 12, 'unimplemented');
    }
  } catch (e) {
    srv.respond(id, new Uint8Array(), INTERNAL, String(e));
  }
}

const loop = await DispatchLoop.start({
  config: { relayMode: 'disabled', bindAddr: '127.0.0.1:0', logLevel: 'warn' },
  services: [INTEROP_ECHO],
  delivery,
  onCall: (ev, srv) => {
    stats.calls++;
    serveCall(ev, srv).then(() => stats.done++);
  },
  onEvent: (ev) => {
    if (ev.kind === DispatchEventKind.Cancelled) {
      /* handlers notice via a negative submit result */
    }
  },
});

// Diagnostics: T4_DEBUG=1 prints native state + JS counters every second;
// T4_DEBUG=nudge also queues one extra T-b drain call when events have sat
// in the queue for 2 s without progress.
if (process.env.T4_DEBUG) {
  let last = -1;
  let still = 0;
  setInterval(() => {
    const st = loop.native.debugState();
    console.error(`DEBUG native=${JSON.stringify(st)} calls=${stats.calls} done=${stats.done}`);
    still = stats.calls === last && st[1] > 0 ? still + 1 : 0;
    last = stats.calls;
    if (process.env.T4_DEBUG === 'nudge' && still === 2) {
      console.error('DEBUG nudge');
      loop.native.debugNudge();
    }
  }, 1000);
}
console.log(`ADDRESS ${loop.address}`);
process.stdin.resume();
await new Promise((res) => {
  process.stdin.on('end', res);
  process.stdin.on('close', res);
});
await loop.close();
process.exit(0);
