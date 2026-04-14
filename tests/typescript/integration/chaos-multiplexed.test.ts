/**
 * Tier-2 chaos tests for the multiplexed-streams architecture (TypeScript).
 *
 * Tier 1 covers spec invariants at the core layer (pool, reactor dispatch,
 * connection lifecycle events). Tier 2 covers the failure modes that only
 * exist at the binding layer: session reap semantics, handler-exception
 * isolation, graveyard enforcement under out-of-order arrival, the
 * per-connection session cap, and cross-connection session isolation.
 *
 * Scope: behavioural chaos — adversarial client behaviour, error paths,
 * and sequencing edge cases. We go deep on TypeScript here because its
 * session lifecycle (AsterServer2 §6/§7.5) is the newest + most complete
 * and we already caught one real bug there during remediation. The same
 * test shapes will port to Python + Java.
 *
 * Design rules:
 *
 *   - Every test asserts on BOTH behaviour (what did happen) and
 *     negative space (what didn't). Every test that could leak state
 *     checks `server.debugConnectionSnapshot()` afterwards.
 *   - The chaos driver runs against real QUIC endpoints — no mocks.
 *   - Each test fixture tears down the server explicitly so the next
 *     test starts with a clean slate.
 *
 * See `ffi_spec/Aster-multiplexed-streams.md` §6, §7.5 for the spec
 * invariants these tests pin.
 */

import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { resolve, dirname } from 'node:path';
import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

import {
  Service,
  Rpc,
  BidiStream,
  WireType,
  JsonCodec,
  AsterClient2,
  AsterServer2,
  ClientSession,
  getServiceInfo,
  RpcError,
  StatusCode,
  type NativeConnection,
  type NativeNode,
  type StartReactorFn,
  type AsterCallFactory,
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
      /* next */
    }
  }
}

const available = !!native;
if (!available) {
  console.warn('Native addon not found — skipping tier-2 chaos tests.');
}

// -- Test wire types ----------------------------------------------------------

@WireType('chaos/BumpRequest')
class BumpRequest {
  message = '';
  constructor(init?: Partial<BumpRequest>) {
    if (init) Object.assign(this, init);
  }
}

@WireType('chaos/BumpResponse')
class BumpResponse {
  reply = '';
  constructor(init?: Partial<BumpResponse>) {
    if (init) Object.assign(this, init);
  }
}

@WireType('chaos/FailRequest')
class FailRequest {
  message = '';
  constructor(init?: Partial<FailRequest>) {
    if (init) Object.assign(this, init);
  }
}

@WireType('chaos/FailResponse')
class FailResponse {
  reply = '';
  constructor(init?: Partial<FailResponse>) {
    if (init) Object.assign(this, init);
  }
}

// -- Chaos service: session-scoped with a counter and a throwing method -----

/**
 * Session-scoped service with three observable pieces of state:
 *   - `counter` — incremented on each `bump`, used to prove per-session
 *     isolation (a throw on one session must not reset another's).
 *   - `onCloseFired` — static sink tracking how many times `onClose`
 *     fires; zeroed by the test fixture before each test.
 *   - `liveInstances` — static counter of currently-alive instances,
 *     incremented in the constructor and decremented in `onClose`,
 *     so tests can assert session instances are reaped.
 */
@Service({ name: 'ChaosSession', version: 1, scoped: 'session' })
class ChaosSessionService {
  static onCloseFired = 0;
  static liveInstances = 0;

  private counter = 0;

  constructor() {
    ChaosSessionService.liveInstances += 1;
  }

  onClose(): void {
    ChaosSessionService.onCloseFired += 1;
    ChaosSessionService.liveInstances -= 1;
  }

  @Rpc({ request: BumpRequest, response: BumpResponse })
  async bump(req: BumpRequest): Promise<BumpResponse> {
    this.counter += 1;
    return new BumpResponse({ reply: `${req.message}:${this.counter}` });
  }

  /** Intentionally throws. Used to prove handler exceptions don't
   *  poison the session instance — a follow-up `bump` on the same
   *  session must still succeed and see the prior counter. */
  @Rpc({ request: FailRequest, response: FailResponse })
  async fail(_req: FailRequest): Promise<FailResponse> {
    throw new Error('chaos/expected-throw');
  }

  @BidiStream({ request: BumpRequest, response: BumpResponse })
  async *bidiBump(
    reqs: AsyncIterable<BumpRequest>,
  ): AsyncGenerator<BumpResponse> {
    for await (const req of reqs) {
      this.counter += 1;
      yield new BumpResponse({ reply: `${req.message}:${this.counter}` });
    }
  }
}

function serviceInfoOf(cls: new (...args: any[]) => object) {
  const info = getServiceInfo(cls);
  if (!info) throw new Error(`${cls.name} has no @Service decoration`);
  return info;
}

// -- Shared fixture harness ---------------------------------------------------

/**
 * Each test gets a fresh server + client pair so one test's session
 * graveyard doesn't leak into the next. Tests that need two clients
 * (cross-connection isolation) get a second client built from the
 * existing server.
 */
interface Fixture {
  serverNode: any;
  clientNode: any;
  server: AsterServer2;
  connection: NativeConnection;
  client: AsterClient2;
  close: () => Promise<void>;
}

async function startFixture(
  maxSessionsPerConnection?: number,
): Promise<Fixture> {
  const RPC_ALPN = Buffer.from('aster/1');
  const serverNode = await native.IrohNode.memoryWithAlpns([RPC_ALPN]);
  const clientNode = await native.IrohNode.memory();
  clientNode.addNodeAddr(serverNode);

  const sessionReg: SessionRegistration = {
    info: serviceInfoOf(ChaosSessionService),
    factory: () => new ChaosSessionService(),
  };

  const server = new AsterServer2({
    node: serverNode as NativeNode,
    startReactor: native.startReactor as StartReactorFn,
    codec: new JsonCodec(),
    session: [sessionReg],
    ...(maxSessionsPerConnection !== undefined
      ? { maxSessionsPerConnection }
      : {}),
  });
  server.start();

  const connection = (await clientNode.connect(
    serverNode.nodeId(),
    RPC_ALPN,
  )) as NativeConnection;
  const client = new AsterClient2({
    connection,
    asterCall: native.AsterCall as AsterCallFactory,
    codec: new JsonCodec(),
  });

  return {
    serverNode,
    clientNode,
    server,
    connection,
    client,
    async close() {
      try {
        await client.close();
      } catch {
        /* best effort */
      }
      server.close();
      try {
        await serverNode.close();
      } catch {
        /* best effort */
      }
      try {
        await clientNode.close();
      } catch {
        /* best effort */
      }
    },
  };
}

// Helper: wait for a predicate to become true, polling at intervals,
// up to a deadline. Returns `true` if satisfied, `false` on timeout.
async function waitFor(
  predicate: () => boolean,
  timeoutMs: number,
  stepMs = 25,
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return true;
    await new Promise((r) => setTimeout(r, stepMs));
  }
  return predicate();
}

describe('tier-2 chaos: multiplexed streams', () => {
  beforeAll(() => {
    if (!available) return;
    ChaosSessionService.onCloseFired = 0;
    ChaosSessionService.liveInstances = 0;
  });

  afterAll(() => {
    if (!available) return;
    // After every test in this suite, all instances should have been
    // reaped via ConnectionClosed. Assert the global live count is
    // zero to catch any leak across the suite as a whole.
    expect(ChaosSessionService.liveInstances).toBe(0);
  });

  // ── 1. Session reap on connection close ────────────────────────────────

  it.skipIf(!available)(
    'reaps all per-connection session state on connection close',
    async () => {
      const f = await startFixture();
      const liveBefore = ChaosSessionService.liveInstances;
      const onCloseBefore = ChaosSessionService.onCloseFired;

      // Open 3 sessions and drive one call on each so they're
      // materialised on the server side.
      const sessions: ClientSession[] = [];
      for (let i = 0; i < 3; i++) {
        const s = await f.client.openSession();
        await s.transport.unary('ChaosSession', 'bump', { message: `s${i}` });
        sessions.push(s);
      }
      expect(ChaosSessionService.liveInstances - liveBefore).toBe(3);

      // Snapshot pre-close: the server has one connection with three
      // active sessions.
      const snapshotPre = f.server.debugConnectionSnapshot();
      expect(snapshotPre.size).toBe(1);
      const [connKey] = Array.from(snapshotPre.keys());
      expect(snapshotPre.get(connKey)!.activeSessionCount).toBe(3);

      // Close client-side sessions first, then tear down the whole
      // connection. Closing ClientSession is a no-op on the wire —
      // it only drops the client-side handle. What reaps server
      // state is the actual QUIC connection close.
      for (const s of sessions) await s.close();
      await f.client.close();

      // Give the reactor a moment to observe the close.
      const reaped = await waitFor(
        () => f.server.debugConnectionSnapshot().size === 0,
        3000,
      );
      expect(reaped).toBe(true);
      expect(f.server.debugConnectionSnapshot().size).toBe(0);

      // Behavioural check: onClose fired exactly once per session,
      // and every instance count returned to its pre-test value.
      expect(ChaosSessionService.onCloseFired - onCloseBefore).toBe(3);
      expect(ChaosSessionService.liveInstances - liveBefore).toBe(0);

      f.server.close();
      await f.serverNode.close();
      await f.clientNode.close();
    },
  );

  // ── 2. Handler exception isolation ─────────────────────────────────────

  it.skipIf(!available)(
    'handler exception on session A does not poison session B or session A itself',
    async () => {
      const f = await startFixture();
      try {
        const sa = await f.client.openSession();
        const sb = await f.client.openSession();

        // Session A: bump twice so counter=2.
        await sa.transport.unary('ChaosSession', 'bump', { message: 'a' });
        await sa.transport.unary('ChaosSession', 'bump', { message: 'a' });
        // Session A: handler throws — must surface as an RpcError
        // without killing the session instance.
        await expect(
          sa.transport.unary('ChaosSession', 'fail', { message: 'a' }),
        ).rejects.toBeInstanceOf(RpcError);

        // Session A: next bump — counter should still be 3 (the
        // throw must not have reset or removed the instance).
        const r = (await sa.transport.unary('ChaosSession', 'bump', {
          message: 'a',
        })) as { reply: string };
        expect(r.reply).toBe('a:3');

        // Session B: must be completely untouched. Its counter is
        // at 0 (zero bumps) so the first bump should see counter=1.
        const rb = (await sb.transport.unary('ChaosSession', 'bump', {
          message: 'b',
        })) as { reply: string };
        expect(rb.reply).toBe('b:1');

        // Both sessions still alive on the server — no side-effect
        // reaping from the throw.
        const snap = f.server.debugConnectionSnapshot();
        const [connKey] = Array.from(snap.keys());
        expect(snap.get(connKey)!.activeSessionCount).toBe(2);

        await sa.close();
        await sb.close();
      } finally {
        await f.close();
      }
    },
  );

  // ── 3. Graveyard enforcement under out-of-order arrival ────────────────

  it.skipIf(!available)(
    'graveyard rejects a session id older than lastOpenedSessionId (§7.5)',
    async () => {
      const f = await startFixture();
      try {
        // Directly construct ClientSession handles for adversarial
        // session ids via `forTest`. Skip the client's monotonic
        // allocator so we can drive out-of-order arrivals at will.
        const s2 = ClientSession.forTest(f.client, f.connection, 2);
        const s1 = ClientSession.forTest(f.client, f.connection, 1);

        // Use session 2 FIRST → server sees sessionId=2 on first
        // stream arrival, creates the session, bumps
        // lastOpenedSessionId to 2.
        const r2 = (await s2.transport.unary('ChaosSession', 'bump', {
          message: 'two',
        })) as { reply: string };
        expect(r2.reply).toBe('two:1');

        // Now try session 1. It is <= lastOpenedSessionId (2) and
        // not in the active set → NOT_FOUND. Per §7.5 this is how
        // the graveyard prevents a replayed / reordered session id
        // from materialising a fresh instance.
        await expect(
          s1.transport.unary('ChaosSession', 'bump', { message: 'one' }),
        ).rejects.toMatchObject({ code: StatusCode.NOT_FOUND });

        // The graveyard rejection must NOT have corrupted session 2.
        // A follow-up call on session 2 still works and its counter
        // continues from 1 → 2.
        const r2b = (await s2.transport.unary('ChaosSession', 'bump', {
          message: 'two',
        })) as { reply: string };
        expect(r2b.reply).toBe('two:2');

        // Snapshot: still exactly one active session (sessionId=2).
        const snap = f.server.debugConnectionSnapshot();
        const [connKey] = Array.from(snap.keys());
        expect(snap.get(connKey)!.activeSessionCount).toBe(1);
        expect(snap.get(connKey)!.lastOpenedSessionId).toBe(2);

        await s2.close();
      } finally {
        await f.close();
      }
    },
  );

  // ── 4a. Session limit, sequenced ───────────────────────────────────────

  it.skipIf(!available)(
    'maxSessionsPerConnection cap enforced when openSession calls are serialised',
    async () => {
      const CAP = 4;
      const EXTRA = 3;
      const f = await startFixture(CAP);
      try {
        // Sequenced openSession → first-call loop. Awaiting between
        // iterations guarantees the server sees sessionIds 1..CAP+EXTRA
        // in strict allocation order, so the cap path is exercised in
        // isolation from the §7.5 graveyard race (which the
        // concurrent-burst test below covers separately).
        const fulfilled: string[] = [];
        const rejectedCodes: StatusCode[] = [];
        for (let i = 0; i < CAP + EXTRA; i++) {
          const s = await f.client.openSession();
          try {
            const r = (await s.transport.unary('ChaosSession', 'bump', {
              message: `seq${i}`,
            })) as { reply: string };
            fulfilled.push(r.reply);
          } catch (e) {
            if (e instanceof RpcError) rejectedCodes.push(e.code);
            else throw e;
          }
        }

        expect(fulfilled.length).toBe(CAP);
        expect(rejectedCodes.length).toBe(EXTRA);
        for (const code of rejectedCodes) {
          expect(code).toBe(StatusCode.RESOURCE_EXHAUSTED);
        }
        // Snapshot: exactly CAP active sessions on the one
        // connection.
        const snap = f.server.debugConnectionSnapshot();
        const [connKey] = Array.from(snap.keys());
        expect(snap.get(connKey)!.activeSessionCount).toBe(CAP);
      } finally {
        await f.close();
      }
    },
  );

  // ── 4b. Session limit under concurrent burst (chaos) ───────────────────

  it.skipIf(!available)(
    'concurrent openSession burst over the cap never exceeds CAP successes; rejections are NOT_FOUND or RESOURCE_EXHAUSTED',
    async () => {
      const CAP = 4;
      const BURST = 12;
      const f = await startFixture(CAP);
      try {
        // Fire `BURST` concurrent openSession → first-call pairs.
        // Because the client allocates session ids deterministically
        // (1..BURST) but QUIC schedules their stream arrivals
        // non-deterministically, the server sees them in some
        // adversarial order. The weaker invariant this test pins:
        //
        //   - At most CAP of them succeed.
        //   - Every rejection is either NOT_FOUND (§7.5 graveyard:
        //     an earlier-numbered id arrived after its successor
        //     bumped `lastOpenedSessionId`) or RESOURCE_EXHAUSTED
        //     (the cap was already full when this id arrived).
        //   - No other error codes surface.
        //
        // The concurrent race is spec-legal per §6 "sessions are
        // created on first stream arrival, not first allocation."
        // Clients that need strict ordering must serialise their
        // openSession calls (see 4a).
        const results = await Promise.allSettled(
          Array.from({ length: BURST }, async (_, i) => {
            const s = await f.client.openSession();
            return await s.transport.unary('ChaosSession', 'bump', {
              message: `burst${i}`,
            });
          }),
        );
        const fulfilled = results.filter((r) => r.status === 'fulfilled');
        const rejected = results.filter(
          (r) => r.status === 'rejected',
        ) as PromiseRejectedResult[];

        expect(fulfilled.length).toBeLessThanOrEqual(CAP);
        expect(fulfilled.length + rejected.length).toBe(BURST);
        for (const r of rejected) {
          expect(r.reason).toBeInstanceOf(RpcError);
          const code = (r.reason as RpcError).code;
          expect([StatusCode.NOT_FOUND, StatusCode.RESOURCE_EXHAUSTED]).toContain(
            code,
          );
        }

        // Snapshot: active sessions matches the fulfilled count —
        // the server didn't leak or double-count.
        const snap = f.server.debugConnectionSnapshot();
        const [connKey] = Array.from(snap.keys());
        expect(snap.get(connKey)!.activeSessionCount).toBe(fulfilled.length);
      } finally {
        await f.close();
      }
    },
  );

  // ── 5. Cross-connection session id isolation ───────────────────────────

  it.skipIf(!available)(
    'same sessionId on two distinct connections resolves to distinct instances',
    async () => {
      const f = await startFixture();
      try {
        // Bring up a second client connection to the same server.
        const RPC_ALPN = Buffer.from('aster/1');
        const clientNode2 = await native.IrohNode.memory();
        clientNode2.addNodeAddr(f.serverNode);
        const conn2 = (await clientNode2.connect(
          f.serverNode.nodeId(),
          RPC_ALPN,
        )) as NativeConnection;
        const client2 = new AsterClient2({
          connection: conn2,
          asterCall: native.AsterCall as AsterCallFactory,
          codec: new JsonCodec(),
        });

        // Both clients force sessionId=1 via forTest.
        const sA = ClientSession.forTest(f.client, f.connection, 1);
        const sB = ClientSession.forTest(client2, conn2, 1);

        // Bump each twice on its own connection.
        await sA.transport.unary('ChaosSession', 'bump', { message: 'A' });
        await sA.transport.unary('ChaosSession', 'bump', { message: 'A' });
        await sB.transport.unary('ChaosSession', 'bump', { message: 'B' });

        // If the server incorrectly keyed sessions by `(sessionId)`
        // alone instead of `(connectionId, sessionId)`, one of
        // these responses would show the OTHER session's counter.
        const rA = (await sA.transport.unary('ChaosSession', 'bump', {
          message: 'A',
        })) as { reply: string };
        const rB = (await sB.transport.unary('ChaosSession', 'bump', {
          message: 'B',
        })) as { reply: string };
        expect(rA.reply).toBe('A:3');
        expect(rB.reply).toBe('B:2');

        // Snapshot: exactly TWO connections, each with one active
        // session.
        const snap = f.server.debugConnectionSnapshot();
        expect(snap.size).toBe(2);
        for (const state of snap.values()) {
          expect(state.activeSessionCount).toBe(1);
          expect(state.lastOpenedSessionId).toBe(1);
        }

        await sA.close();
        await sB.close();
        await client2.close();
        await clientNode2.close();
      } finally {
        await f.close();
      }
    },
  );
});
