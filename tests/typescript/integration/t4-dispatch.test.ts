/**
 * T4: decorated TS services on `aster::rpc::Server` (`RpcServer` in
 * `dispatch.ts`) and the client on the pooled `RpcConnection`
 * (`IrohTransport`), over real QUIC between two nodes.
 *
 * Covers: the four patterns through typed clients, the real service version
 * on the wire, sessions (instance per session, close hooks on disconnect,
 * ids from the shared core counter), CallContext (metadata, peer,
 * attributes), the design §2.7 error mapping, Rust-owned deadlines and
 * `ctx.signal`, client `AbortSignal` cancel, Gate 3 `requires` through both
 * attribute bridges, non-RPC ALPN routing, and the `ring` delivery. Every
 * test ends with an empty call table.
 */

import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  ServiceRegistry,
  RpcServer,
  IrohTransport,
  createClient,
  CallContext,
  RpcError,
  StatusCode,
  GatePolicy,
  NotFoundError,
  type Codec,
} from '@aster-rpc/aster';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.ts';

const native = tryLoadNative() as any;
const available = !!native?.RpcConnection;

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0', logLevel: 'warn' } as const;
const RPC_ALPN = Buffer.from('aster/1');
const SIDE_ALPN = Buffer.from('t4/side');

/** JSON payloads: enough for the dispatch path, and easy to corrupt. */
const JSON_CODEC: Codec = {
  encode: (x: unknown) => Buffer.from(JSON.stringify(x ?? null)),
  decode: (b: Uint8Array) => JSON.parse(Buffer.from(b).toString('utf8')),
};

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function until(cond: () => boolean, ms = 5000): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error('condition not met in time');
    await sleep(10);
  }
}

// ── services ────────────────────────────────────────────────────────────────

const seen: { ctx?: CallContext; aborted?: unknown; sessionsClosed: number } = { sessionsClosed: 0 };

@Service({ name: 't4.Echo', version: 1 })
class EchoV1 {
  @Rpc()
  async echo(req: { msg: string }, ctx: CallContext): Promise<unknown> {
    seen.ctx = ctx;
    return { msg: req.msg, v: 1, meta: ctx.metadata, peer: ctx.peer, callId: ctx.callId };
  }

  @ServerStream()
  async *count(req: { n: number }): AsyncGenerator<unknown> {
    for (let i = 0; i < req.n; i++) yield { i };
  }

  @ClientStream()
  async sum(reqs: AsyncIterable<{ x: number }>): Promise<unknown> {
    let total = 0;
    let n = 0;
    for await (const r of reqs) {
      total += r.x;
      n++;
    }
    return { total, n };
  }

  @BidiStream()
  async *chat(reqs: AsyncIterable<{ msg: string }>): AsyncGenerator<unknown> {
    for await (const r of reqs) yield { msg: `re: ${r.msg}` };
  }

  @Rpc()
  async fail(req: { kind: string }): Promise<unknown> {
    if (req.kind === 'rpc') throw new NotFoundError('no such thing', { key: 'k1' });
    if (req.kind === 'error') throw new Error('boom');
    return {};
  }

  /** Sleeps past its deadline; records how its signal ended. */
  @Rpc({ timeout: 0.3 })
  async slow(_req: unknown, ctx: CallContext): Promise<unknown> {
    seen.aborted = undefined;
    await new Promise<void>((resolve) => {
      ctx.signal.addEventListener('abort', () => {
        seen.aborted = ctx.signal.reason;
        resolve();
      });
      setTimeout(resolve, 5000);
    });
    return { late: true };
  }

  /** Never yields until cancelled. */
  @ServerStream()
  async *hang(_req: unknown, ctx: CallContext): AsyncGenerator<unknown> {
    seen.aborted = undefined;
    yield { first: true };
    await new Promise<void>((resolve) => {
      ctx.signal.addEventListener('abort', () => {
        seen.aborted = ctx.signal.reason;
        resolve();
      });
    });
  }
}

@Service({ name: 't4.Echo', version: 2 })
class EchoV2 {
  @Rpc()
  async echo(req: { msg: string }): Promise<unknown> {
    return { msg: req.msg, v: 2 };
  }
}

let nextCounter = 0;

@Service({ name: 't4.Counter', version: 1, scoped: 'session' })
class Counter {
  private n = 0;
  readonly id = ++nextCounter;

  @Rpc()
  async incr(_req: unknown): Promise<unknown> {
    this.n++;
    return { n: this.n, id: this.id };
  }

  close(): void {
    seen.sessionsClosed++;
  }
}

@Service({ name: 't4.Guarded', version: 1 })
class Guarded {
  calls = 0;
  @Rpc({ requires: 'admin' })
  async secret(_req: unknown, ctx: CallContext): Promise<unknown> {
    this.calls++;
    return { role: ctx.attributes['aster.role'] ?? null };
  }

  @Rpc()
  async open(_req: unknown): Promise<unknown> {
    return { ok: true };
  }
}

// ── fixture ─────────────────────────────────────────────────────────────────

interface Fixture {
  serverNode: any;
  clientNode: any;
  server: RpcServer;
  conns: any[];
}

async function fixture(opts: {
  services: object[];
  delivery?: 'tsfn-batched' | 'tsfn' | 'ring';
  gate?: GatePolicy;
  onNonRpc?: (alpn: string, conn: any) => void;
}): Promise<Fixture> {
  const serverNode = await native.Node.start({ ...OFFLINE, alpns: [RPC_ALPN, SIDE_ALPN] });
  const clientNode = await native.Node.start(OFFLINE);
  clientNode.addNodeAddr(serverNode);
  const registry = new ServiceRegistry();
  for (const s of opts.services) registry.register(s);
  const server = new RpcServer({
    registry,
    codec: JSON_CODEC,
    delivery: opts.delivery,
    gateAttributes: opts.gate,
    onNonRpc: opts.onNonRpc,
  });
  await server.start(serverNode);
  return { serverNode, clientNode, server, conns: [] };
}

async function connect(f: Fixture): Promise<IrohTransport> {
  const conn = await f.clientNode.connect(f.serverNode.nodeId(), RPC_ALPN);
  f.conns.push(conn);
  return new IrohTransport(conn, JSON_CODEC);
}

async function teardown(f: Fixture | undefined): Promise<void> {
  if (!f) return;
  for (const c of f.conns) {
    try { c.close(0, 'done'); } catch { /* ignore */ }
  }
  await f.server.close();
  await f.serverNode.close();
  await f.clientNode.close();
}

async function expectCode(p: Promise<unknown>, code: number): Promise<RpcError> {
  try {
    await p;
  } catch (e) {
    expect(e).toBeInstanceOf(RpcError);
    expect((e as RpcError).code).toBe(code);
    return e as RpcError;
  }
  throw new Error(`expected status ${code}, call succeeded`);
}

// ── tests ───────────────────────────────────────────────────────────────────

describe.skipIf(!available)('T4: RpcServer on dynamic dispatch + RpcConnection client', () => {
  let f: Fixture;
  let t: IrohTransport;

  beforeAll(async () => {
    f = await fixture({ services: [new EchoV1(), new EchoV2(), new Counter()] });
    t = await connect(f);
  });

  afterAll(async () => {
    await teardown(f);
  });

  it('typed client: all four patterns, with the real service version', async () => {
    const v1 = createClient(EchoV1, t) as any;
    const v2 = createClient(EchoV2, t) as any;
    expect(await v1.echo({ msg: 'a' })).toMatchObject({ msg: 'a', v: 1 });
    expect(await v2.echo({ msg: 'b' })).toEqual({ msg: 'b', v: 2 });

    const items: number[] = [];
    for await (const x of v1.count({ n: 5 }) as AsyncIterable<{ i: number }>) items.push(x.i);
    expect(items).toEqual([0, 1, 2, 3, 4]);

    async function* xs() {
      for (let i = 1; i <= 100; i++) yield { x: i };
    }
    expect(await v1.sum(xs())).toEqual({ total: 5050, n: 100 });

    const ch = v1.chat();
    await ch.send({ msg: 'one' });
    await ch.send({ msg: 'two' });
    await ch.close();
    const replies: string[] = [];
    for await (const r of ch) replies.push((r as any).msg);
    expect(replies).toEqual(['re: one', 're: two']);
    await until(() => f.server.liveCalls === 0);
  });

  it('CallContext: metadata, peer and wire call id reach the handler', async () => {
    const r: any = await t.unary('t4.Echo', 'echo', { msg: 'm' }, { metadata: { 'x-trace': 'abc' }, callId: 77 });
    expect(r.meta).toEqual({ 'x-trace': 'abc' });
    expect(r.peer).toBe(f.clientNode.nodeId());
    expect(r.callId).toBe('77');
    expect(seen.ctx!.pattern).toBe('unary');
    expect(seen.ctx!.peerAddr?.host).toBe('127.0.0.1');
  });

  it('error mapping (design §2.7)', async () => {
    const e = await expectCode(t.unary('t4.Echo', 'fail', { kind: 'rpc' }), StatusCode.NOT_FOUND);
    expect(e.message).toContain('no such thing');
    expect(e.details).toEqual({ key: 'k1' });
    const e2 = await expectCode(t.unary('t4.Echo', 'fail', { kind: 'error' }), StatusCode.INTERNAL);
    expect(e2.message).toContain('boom');
    // A payload that does not decode.
    const bad = { encode: () => Buffer.from('{not json'), decode: JSON_CODEC.decode };
    const raw = new IrohTransport(t.conn, bad);
    await expectCode(raw.unary('t4.Echo', 'echo', null), StatusCode.INVALID_ARGUMENT);
    // Routing failures come from Rust.
    await expectCode(t.unary('t4.Echo', 'nope', {}), StatusCode.UNIMPLEMENTED);
    await expectCode(t.unary('t4.Echo', 'echo', {}, { version: 9 }), StatusCode.NOT_FOUND);
    await expectCode(t.unary('t4.Missing', 'x', {}), StatusCode.NOT_FOUND);
    await until(() => f.server.liveCalls === 0);
  });

  it('deadlines are Rust\'s: DEADLINE_EXCEEDED to the client, ctx.signal aborts', async () => {
    // Method default_timeout 0.3 s.
    await expectCode(t.unary('t4.Echo', 'slow', {}), StatusCode.DEADLINE_EXCEEDED);
    await until(() => seen.aborted !== undefined);
    expect((seen.aborted as RpcError).code).toBe(StatusCode.DEADLINE_EXCEEDED);
    await until(() => f.server.liveCalls === 0);
  });

  it('client deadline (deadlineSecs) is enforced client-side', async () => {
    const t0 = Date.now();
    await expectCode(t.unary('t4.Echo', 'slow', {}, { deadlineSecs: 0.1 }), StatusCode.DEADLINE_EXCEEDED);
    expect(Date.now() - t0).toBeLessThan(290);
    await until(() => f.server.liveCalls === 0);
  });

  it('AbortSignal cancels a server stream; the handler sees its signal abort', async () => {
    const ac = new AbortController();
    const it = t.serverStream('t4.Echo', 'hang', {}, { signal: ac.signal })[Symbol.asyncIterator]();
    expect((await it.next()).value).toEqual({ first: true });
    const pending = it.next();
    ac.abort();
    await expectCode(pending, StatusCode.CANCELLED);
    await until(() => seen.aborted !== undefined);
    expect((seen.aborted as RpcError).code).toBe(StatusCode.CANCELLED);
    await until(() => f.server.liveCalls === 0);
  });

  it('AbortSignal cancels a unary call', async () => {
    const ac = new AbortController();
    const p = t.unary('t4.Echo', 'slow', {}, { signal: ac.signal });
    setTimeout(() => ac.abort(), 30);
    await expectCode(p, StatusCode.CANCELLED);
    const pre = new AbortController();
    pre.abort();
    await expectCode(t.unary('t4.Echo', 'echo', { msg: 'x' }, { signal: pre.signal }), StatusCode.CANCELLED);
    await until(() => f.server.liveCalls === 0);
  });

  it('breaking out of a server stream cancels it', async () => {
    for await (const _ of t.serverStream('t4.Echo', 'hang', {})) break;
    await until(() => f.server.liveCalls === 0);
  });

  it('sessions: one instance per session, ids from the shared counter, close on disconnect', async () => {
    const t2 = await connect(f);
    const a = t2.openSession();
    const b = t2.openSession();
    expect(b.sessionId).toBe(a.sessionId + 1);
    const ra1: any = await a.unary('t4.Counter', 'incr', {});
    const ra2: any = await a.unary('t4.Counter', 'incr', {});
    const rb1: any = await b.unary('t4.Counter', 'incr', {});
    expect([ra1.n, ra2.n, rb1.n]).toEqual([1, 2, 1]);
    expect(ra1.id).toBe(ra2.id);
    expect(rb1.id).not.toBe(ra1.id);
    // A session service without a session id.
    await expectCode(t2.unary('t4.Counter', 'incr', {}), StatusCode.FAILED_PRECONDITION);
    // A shared service with a session id.
    await expectCode(a.unary('t4.Echo', 'echo', { msg: 'x' }), StatusCode.FAILED_PRECONDITION);
    const before = seen.sessionsClosed;
    t2.conn.close(0, 'bye');
    await until(() => seen.sessionsClosed === before + 2);
  });
});

describe.skipIf(!available)('T4: Gate 3 requires, attribute bridges, non-RPC ALPNs, ring delivery', () => {
  let f: Fixture | undefined;
  afterAll(async () => {
    await teardown(f);
  });

  it('requires: denied without attributes; setPeerAttributes admits', async () => {
    const guarded = new Guarded();
    f = await fixture({ services: [guarded], delivery: 'ring' });
    const t = await connect(f);
    const me = f.clientNode.nodeId();
    expect(await t.unary('t4.Guarded', 'open', {})).toEqual({ ok: true });
    await expectCode(t.unary('t4.Guarded', 'secret', {}), StatusCode.PERMISSION_DENIED);
    expect(guarded.calls).toBe(0);
    f.server.setPeerAttributes(me, { 'aster.role': 'admin' });
    expect(await t.unary('t4.Guarded', 'secret', {})).toEqual({ role: 'admin' });
    expect(guarded.calls).toBe(1);
    f.server.setPeerAttributes(me, null);
    await expectCode(t.unary('t4.Guarded', 'secret', {}), StatusCode.PERMISSION_DENIED);
    expect(guarded.calls).toBe(1);
    await until(() => f!.server.liveCalls === 0);
    await teardown(f);
    f = undefined;
  });

  it('requires: Gate 3 reads the gate\'s admission store (frozen admission bridge)', async () => {
    const gate = GatePolicy.openDev();
    const guarded = new Guarded();
    f = await fixture({ services: [guarded], gate });
    const t = await connect(f);
    const me = f.clientNode.nodeId();
    await expectCode(t.unary('t4.Guarded', 'secret', {}), StatusCode.PERMISSION_DENIED);
    expect(guarded.calls).toBe(0);
    gate.admit(me, { attributes: { 'aster.role': 'admin' } });
    expect(await t.unary('t4.Guarded', 'secret', {})).toEqual({ role: 'admin' });
    expect(guarded.calls).toBe(1);
    gate.revoke(me);
    await expectCode(t.unary('t4.Guarded', 'secret', {}), StatusCode.PERMISSION_DENIED);
    expect(guarded.calls).toBe(1);
    await teardown(f);
    f = undefined;
  });

  it('non-RPC ALPNs reach onNonRpc', async () => {
    const got: string[] = [];
    f = await fixture({
      services: [new Guarded()],
      onNonRpc: (alpn, conn) => {
        got.push(`${alpn}:${conn.remoteNodeId()}`);
        conn.close(0, 'ok');
      },
    });
    const c = await f.clientNode.connect(f.serverNode.nodeId(), SIDE_ALPN);
    await until(() => got.length === 1);
    expect(got[0]).toBe(`t4/side:${f.clientNode.nodeId()}`);
    try { c.close(0, 'x'); } catch { /* ignore */ }
  });
});
