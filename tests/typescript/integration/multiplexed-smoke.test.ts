/**
 * Multiplexed-streams smoke test (Session 1 acceptance).
 *
 * Exercises `AsterClient2` + `AsterServer2` end-to-end over real QUIC:
 *   1. All four RPC patterns on a SHARED-pool transport (sessionId=0).
 *   2. All four RPC patterns on a SESSION-bound transport (sessionId>0).
 *   3. Two concurrent sessions — each gets its own server-side instance
 *      (§6 lookup-or-create).
 *   4. A server-stream call running in parallel with a unary call on the
 *      same session — proves streaming substreams don't block the
 *      session's main stream (§4 scenario 4).
 *
 * The napi reactor + AsterCall surface ships with this session; this
 * test is the smoke gate that those Rust additions are wired correctly.
 */

import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { resolve, dirname } from 'node:path';
import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

import {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  WireType,
  JsonCodec,
  AsterClient2,
  AsterServer2,
  getServiceInfo,
  type NativeConnection,
  type NativeNode,
  type StartReactorFn,
  type AsterCallFactory,
  type SharedRegistration,
  type SessionRegistration,
} from '@aster-rpc/aster';

const __dirname = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

// -- Load native addon --------------------------------------------------------

const candidates = [
  resolve(__dirname, '../../../bindings/typescript/native/aster-transport.darwin-arm64.node'),
  resolve(__dirname, '../../../bindings/typescript/native/aster-transport.darwin-x64.node'),
  resolve(__dirname, '../../../bindings/typescript/native/aster-transport.linux-x64-gnu.node'),
  resolve(__dirname, '../../../bindings/typescript/native/aster-transport.linux-arm64-gnu.node'),
];

let native: any;
for (const path of candidates) {
  if (existsSync(path)) {
    try {
      native = require(path);
      break;
    } catch {
      /* next candidate */
    }
  }
}

const available = !!native;
if (!available) {
  console.warn('Native addon not found — skipping multiplexed smoke tests.');
}

// -- Test wire types ----------------------------------------------------------

@WireType('smoke/EchoRequest')
class EchoRequest {
  message = '';
  constructor(init?: Partial<EchoRequest>) {
    if (init) Object.assign(this, init);
  }
}

@WireType('smoke/EchoResponse')
class EchoResponse {
  reply = '';
  constructor(init?: Partial<EchoResponse>) {
    if (init) Object.assign(this, init);
  }
}

@WireType('smoke/CountRequest')
class CountRequest {
  n = 0;
  constructor(init?: Partial<CountRequest>) {
    if (init) Object.assign(this, init);
  }
}

@WireType('smoke/CountValue')
class CountValue {
  value = 0;
  constructor(init?: Partial<CountValue>) {
    if (init) Object.assign(this, init);
  }
}

@WireType('smoke/SumResponse')
class SumResponse {
  total = 0;
  constructor(init?: Partial<SumResponse>) {
    if (init) Object.assign(this, init);
  }
}

// -- Services -----------------------------------------------------------------

@Service({ name: 'SharedSmoke', version: 1, scoped: 'shared' })
class SharedSmokeService {
  @Rpc({ request: EchoRequest, response: EchoResponse })
  async echo(req: EchoRequest): Promise<EchoResponse> {
    return new EchoResponse({ reply: `shared: ${req.message}` });
  }

  @ServerStream({ request: CountRequest, response: CountValue })
  async *count(req: CountRequest): AsyncGenerator<CountValue> {
    for (let i = 1; i <= req.n; i++) {
      yield new CountValue({ value: i });
    }
  }

  @ClientStream({ request: CountValue, response: SumResponse })
  async sum(reqs: AsyncIterable<CountValue>): Promise<SumResponse> {
    let total = 0;
    for await (const v of reqs) total += v.value;
    return new SumResponse({ total });
  }

  @BidiStream({ request: EchoRequest, response: EchoResponse })
  async *bidiEcho(reqs: AsyncIterable<EchoRequest>): AsyncGenerator<EchoResponse> {
    for await (const req of reqs) {
      yield new EchoResponse({ reply: `bidi: ${req.message}` });
    }
  }
}

/**
 * Session-scoped counter service. Each session gets a fresh instance
 * so incrementing in one session leaves other sessions untouched —
 * the isolation test relies on this.
 */
@Service({ name: 'SessionSmoke', version: 1, scoped: 'session' })
class SessionSmokeService {
  // Session-scoped state. Per-session factory handed to AsterServer2
  // returns a new instance per (connectionId, sessionId), so each
  // session sees its own counter.
  private counter = 0;

  @Rpc({ request: EchoRequest, response: EchoResponse })
  async bump(req: EchoRequest): Promise<EchoResponse> {
    this.counter += 1;
    return new EchoResponse({ reply: `${req.message}:${this.counter}` });
  }

  @ClientStream({ request: CountValue, response: SumResponse })
  async sum(reqs: AsyncIterable<CountValue>): Promise<SumResponse> {
    let total = 0;
    for await (const v of reqs) total += v.value;
    return new SumResponse({ total });
  }

  @BidiStream({ request: EchoRequest, response: EchoResponse })
  async *bidiEcho(reqs: AsyncIterable<EchoRequest>): AsyncGenerator<EchoResponse> {
    for await (const req of reqs) {
      this.counter += 1;
      yield new EchoResponse({ reply: `${req.message}:${this.counter}` });
    }
  }
}

// -- Harness ------------------------------------------------------------------

function serviceInfoOf(cls: new (...args: any[]) => object) {
  const info = getServiceInfo(cls);
  if (!info) throw new Error(`${cls.name} has no @Service decoration`);
  return info;
}

describe('multiplexed-streams smoke (Session 1)', () => {
  let serverNode: any;
  let clientNode: any;
  let server: AsterServer2 | null = null;
  let sharedConnection: NativeConnection | null = null;
  let client2: AsterClient2 | null = null;

  beforeAll(async () => {
    if (!available) return;

    const RPC_ALPN = Buffer.from('aster/1');

    // Two in-memory nodes. Server registers the Aster ALPN so the
    // reactor accept loop can receive RPC connections.
    serverNode = await native.IrohNode.memoryWithAlpns([RPC_ALPN]);
    clientNode = await native.IrohNode.memory();
    clientNode.addNodeAddr(serverNode);

    // Shared + session service registrations. The SESSION service uses
    // a factory so each (connectionId, sessionId) gets its own
    // instance — this is what the isolation test depends on.
    const sharedInstance = new SharedSmokeService();
    const sharedReg: SharedRegistration = {
      info: serviceInfoOf(SharedSmokeService),
      instance: sharedInstance,
    };
    const sessionReg: SessionRegistration = {
      info: serviceInfoOf(SessionSmokeService),
      factory: () => new SessionSmokeService(),
    };

    server = new AsterServer2({
      node: serverNode as NativeNode,
      startReactor: native.startReactor as StartReactorFn,
      codec: new JsonCodec(),
      shared: [sharedReg],
      session: [sessionReg],
    });
    server.start();

    // Open one QUIC connection for the whole test suite — session tests
    // and the parallel-streaming test must all hit the same reactor-side
    // ConnectionState, which is keyed by connectionId.
    sharedConnection = (await clientNode.connect(serverNode.nodeId(), RPC_ALPN)) as NativeConnection;
    client2 = new AsterClient2({
      connection: sharedConnection,
      asterCall: native.AsterCall as AsterCallFactory,
      codec: new JsonCodec(),
    });
  });

  afterAll(async () => {
    if (!available) return;
    try {
      await client2?.close();
    } catch {
      /* best effort */
    }
    server?.close();
    try {
      await serverNode?.close();
    } catch {
      /* best effort */
    }
    try {
      await clientNode?.close();
    } catch {
      /* best effort */
    }
  });

  // ── 1. Four patterns over the SHARED pool (sessionId = 0) ──────────────

  it.skipIf(!available)('SHARED: unary', async () => {
    const transport = client2!.sharedTransport();
    const response = (await transport.unary('SharedSmoke', 'echo', {
      message: 'one',
    })) as { reply: string };
    expect(response.reply).toBe('shared: one');
  });

  it.skipIf(!available)('SHARED: serverStream', async () => {
    const transport = client2!.sharedTransport();
    const values: number[] = [];
    for await (const item of transport.serverStream('SharedSmoke', 'count', { n: 4 })) {
      values.push((item as { value: number }).value);
    }
    expect(values).toEqual([1, 2, 3, 4]);
  });

  it.skipIf(!available)('SHARED: clientStream', async () => {
    const transport = client2!.sharedTransport();
    async function* requests(): AsyncGenerator<{ value: number }> {
      for (let i = 1; i <= 5; i++) yield { value: i };
    }
    const response = (await transport.clientStream(
      'SharedSmoke',
      'sum',
      requests(),
    )) as { total: number };
    expect(response.total).toBe(15);
  });

  it.skipIf(!available)('SHARED: bidiStream', async () => {
    const transport = client2!.sharedTransport();
    const channel = transport.bidiStream('SharedSmoke', 'bidiEcho');
    const replies: string[] = [];
    const reader = (async () => {
      for await (const item of channel) {
        replies.push((item as { reply: string }).reply);
      }
    })();
    await channel.send({ message: 'a' });
    await channel.send({ message: 'b' });
    await channel.close();
    await reader;
    expect(replies).toEqual(['bidi: a', 'bidi: b']);
  });

  // ── 2. Four patterns over a SESSION ─────────────────────────────────────

  it.skipIf(!available)('SESSION: unary via openSession', async () => {
    const session = await client2!.openSession();
    const transport = session.transport;
    const r1 = (await transport.unary('SessionSmoke', 'bump', { message: 'x' })) as {
      reply: string;
    };
    const r2 = (await transport.unary('SessionSmoke', 'bump', { message: 'x' })) as {
      reply: string;
    };
    // Same session → counter continues.
    expect(r1.reply).toBe('x:1');
    expect(r2.reply).toBe('x:2');
    await session.close();
  });

  it.skipIf(!available)('SESSION: clientStream + bidiStream on same session', async () => {
    const session = await client2!.openSession();
    const transport = session.transport;

    async function* values(): AsyncGenerator<{ value: number }> {
      for (let i = 1; i <= 3; i++) yield { value: i };
    }
    const sumResult = (await transport.clientStream(
      'SessionSmoke',
      'sum',
      values(),
    )) as { total: number };
    expect(sumResult.total).toBe(6);

    const channel = transport.bidiStream('SessionSmoke', 'bidiEcho');
    const out: string[] = [];
    const reader = (async () => {
      for await (const item of channel) out.push((item as { reply: string }).reply);
    })();
    await channel.send({ message: 'alpha' });
    await channel.send({ message: 'beta' });
    await channel.close();
    await reader;
    // Counter is fresh on this session; bidiEcho increments per message.
    expect(out).toEqual(['alpha:1', 'beta:2']);

    await session.close();
  });

  // ── 3. Two concurrent sessions — each gets its own instance ─────────────

  it.skipIf(!available)('SESSION: two sessions have independent state', async () => {
    const s1 = await client2!.openSession();
    const s2 = await client2!.openSession();
    expect(s1.sessionId).not.toBe(s2.sessionId);

    const r1a = (await s1.transport.unary('SessionSmoke', 'bump', { message: 'a' })) as {
      reply: string;
    };
    const r1b = (await s1.transport.unary('SessionSmoke', 'bump', { message: 'a' })) as {
      reply: string;
    };
    const r2a = (await s2.transport.unary('SessionSmoke', 'bump', { message: 'b' })) as {
      reply: string;
    };

    // s1 counter advances to 2; s2 has its own fresh counter at 1.
    expect(r1a.reply).toBe('a:1');
    expect(r1b.reply).toBe('a:2');
    expect(r2a.reply).toBe('b:1');

    await s1.close();
    await s2.close();
  });

  // ── 4. Intra-session streaming + unary parallelism (spec §4.4) ──────────
  //
  // With the default `session_pool_size=1`, a naive implementation would
  // route streaming calls through the same single-slot pool as unary
  // calls, so a long-running streaming call on a session would block
  // concurrent unary calls on the same session. Spec §3 line 65
  // specifies that streaming substreams bypass the pool entirely —
  // this test is the regression gate for that guarantee.
  //
  // Shape: hold a bidi channel open, then issue 5 unary calls on the
  // same session. If streaming counted against the session pool, the
  // unary calls would queue on POOL_FULL for the full
  // stream_acquire_timeout and then error. With the bypass they
  // complete immediately.

  it.skipIf(!available)(
    'SESSION: two server-streams run in parallel on two sessions, same connection',
    async () => {
      const s1 = await client2!.openSession();
      const s2 = await client2!.openSession();

      // Warm up each session with a sequenced unary so the server
      // registers them in monotonic-allocation order. Without this,
      // the two `bidiStream` first-send races in `Promise.all` can
      // arrive at the server in reverse order, and the later-numbered
      // session would advance `lastOpenedSessionId` past the
      // earlier-numbered one, tripping the §7.5 graveyard check. The
      // spec says "sessions are created on first stream arrival", so
      // this is correct server behaviour — the test must sequence
      // first-arrival if it wants inter-session parallelism later.
      await s1.transport.unary('SessionSmoke', 'bump', { message: 'warmup' });
      await s2.transport.unary('SessionSmoke', 'bump', { message: 'warmup' });

      // Each session has its own SessionSmokeService instance, so its
      // counter started at 0 and is now 1 after the warmup. We
      // exercise the bidi pattern in parallel to prove both sessions'
      // streaming substreams make progress concurrently without
      // blocking each other.
      const c1 = s1.transport.bidiStream('SessionSmoke', 'bidiEcho');
      const c2 = s2.transport.bidiStream('SessionSmoke', 'bidiEcho');

      const out1: string[] = [];
      const out2: string[] = [];
      const r1 = (async () => {
        for await (const item of c1) out1.push((item as { reply: string }).reply);
      })();
      const r2 = (async () => {
        for await (const item of c2) out2.push((item as { reply: string }).reply);
      })();

      // Interleave sends on both sessions. If the two sessions shared a
      // single underlying stream (or if the streaming substreams were
      // serialised through a single pool slot) the second pair of
      // sends would wait on the first to drain. Instead we expect
      // both reader loops to make progress concurrently.
      await Promise.all([c1.send({ message: 'one' }), c2.send({ message: 'alpha' })]);
      await Promise.all([c1.send({ message: 'two' }), c2.send({ message: 'beta' })]);
      await Promise.all([c1.close(), c2.close()]);
      await Promise.all([r1, r2]);

      // After the warmup bump (counter=1), the bidi messages increment
      // from 2. Each session has its own counter, so both are at 2.
      expect(out1).toEqual(['one:2', 'two:3']);
      expect(out2).toEqual(['alpha:2', 'beta:3']);

      await s1.close();
      await s2.close();
    },
  );

  // ── 5. Spec §4.4 regression: streaming + unary on the same session ────

  it.skipIf(!available)(
    'SESSION: streaming call does not block concurrent unary calls on same session (§4.4)',
    async () => {
      const session = await client2!.openSession();
      const transport = session.transport;

      // Open a bidi channel and hold it open without closing so the
      // streaming substream stays live for the duration of the unary
      // calls below. If the substream counted against the session
      // pool (default size 1), every unary acquire would queue on
      // POOL_FULL and eventually time out.
      const channel = transport.bidiStream('SessionSmoke', 'bidiEcho');
      const replies: string[] = [];
      const reader = (async () => {
        for await (const item of channel) replies.push((item as { reply: string }).reply);
      })();
      await channel.send({ message: 'streaming' });

      // Fire 5 unary calls on the same session concurrently. These
      // must all complete without being starved by the live bidi
      // substream.
      const unaries = await Promise.all(
        Array.from({ length: 5 }, (_, i) =>
          transport.unary('SessionSmoke', 'bump', { message: `u${i}` }),
        ),
      );
      // bidiEcho's send consumed counter=1; unary bumps see counters
      // 2..6, regardless of their dispatch order.
      const seen = new Set(
        (unaries as { reply: string }[]).map((r) => Number(r.reply.split(':')[1])),
      );
      expect(seen).toEqual(new Set([2, 3, 4, 5, 6]));

      await channel.close();
      await reader;
      expect(replies).toEqual(['streaming:1']);

      await session.close();
    },
  );
});
