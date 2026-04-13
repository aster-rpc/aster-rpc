package site.aster.examples.missioncontrol;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import site.aster.client.AsterClient;
import site.aster.client.ClientSession;
import site.aster.codec.ForyCodec;
import site.aster.examples.missioncontrol.types.Assignment;
import site.aster.examples.missioncontrol.types.Heartbeat;
import site.aster.handle.IrohConnection;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * Tier-2 chaos tests for the multiplexed-streams binding layer (Java port).
 *
 * <p>Mirrors {@code tests/typescript/integration/chaos-multiplexed.test.ts}. Where the TS suite
 * uses a dedicated {@code ChaosSessionService}, the Java port reuses {@link AgentSession} and its
 * test-only {@code chaosFail} method — the invariants are the same regardless of which service
 * shape drives them.
 *
 * <p>Coverage:
 *
 * <ol>
 *   <li>Session reap on connection close — after the client closes, the server's per-connection
 *       state map must be empty (spec §7.5).
 *   <li>Handler exception isolation — a thrown {@link RuntimeException} on session A must not
 *       poison session A's instance for subsequent calls, and must not leak state to session B.
 *   <li>Graveyard enforcement (§7.5) under out-of-order arrival — session 2 used before session 1
 *       must cause session 1 to be rejected with {@code NOT_FOUND}.
 *   <li>Session cap, sequenced — exactly {@code CAP} openSession/call pairs succeed; the rest fail
 *       with {@code RESOURCE_EXHAUSTED}.
 *   <li>Session cap under concurrent burst — weaker invariant: at most {@code CAP} succeed, and
 *       every rejection is either {@code NOT_FOUND} (graveyard race) or {@code RESOURCE_EXHAUSTED}
 *       (cap).
 *   <li>Cross-connection session id isolation — the same {@code sessionId} on two distinct
 *       connections must resolve to two distinct server-side instances.
 * </ol>
 */
@Timeout(value = 60)
final class ChaosMultiplexedTest {

  // ── 1. Session reap on connection close ────────────────────────────────

  @Test
  void reapsPerConnectionStateOnConnectionClose() throws Exception {
    try (Fixture f = Fixture.start()) {
      // Open 3 sessions, drive one call on each so they're materialised server-side.
      List<ClientSession> sessions = new ArrayList<>();
      for (int i = 0; i < 3; i++) {
        ClientSession s = f.client.openSession(f.serverAddr).get(15, TimeUnit.SECONDS);
        s.<Heartbeat, Assignment>call(
                AgentSessionDispatcher.SERVICE_NAME,
                "register",
                new Heartbeat("reap-" + i, List.of("cpu"), 0.1d),
                Assignment.class)
            .get(15, TimeUnit.SECONDS);
        sessions.add(s);
      }

      var preSnapshot = f.server.debugConnectionSnapshot();
      assertEquals(1, preSnapshot.size(), "expected exactly one connection pre-close");
      long connId = preSnapshot.keySet().iterator().next();
      assertEquals(3, preSnapshot.get(connId).activeSessionCount());

      // Close client sessions then the client itself — the latter is what tears down the QUIC
      // connection; ClientSession.close is a no-op on the wire.
      for (ClientSession s : sessions) {
        s.close();
      }
      f.client.close();

      // Poll for reap with a generous deadline. The reactor emits ConnectionClosed on the poll
      // thread, handled asynchronously.
      boolean reaped = waitFor(() -> f.server.debugConnectionSnapshot().isEmpty(), 5000, 50);
      assertTrue(reaped, "server did not reap connection state within 5s");
      assertEquals(0, f.server.debugConnectionSnapshot().size());
    }
  }

  // ── 2. Handler exception isolation ─────────────────────────────────────

  @Test
  void handlerExceptionDoesNotPoisonSessionOrLeakAcrossSessions() throws Exception {
    try (Fixture f = Fixture.start();
        ClientSession sa =
            f.client.openSession(f.serverAddr).orTimeout(15, TimeUnit.SECONDS).get();
        ClientSession sb =
            f.client.openSession(f.serverAddr).orTimeout(15, TimeUnit.SECONDS).get()) {

      // Session A: two successful register calls. `register` with gpu → taskId="train-42".
      Assignment a1 =
          sa.<Heartbeat, Assignment>call(
                  AgentSessionDispatcher.SERVICE_NAME,
                  "register",
                  new Heartbeat("agent-A", List.of("gpu"), 0.5d),
                  Assignment.class)
              .get(15, TimeUnit.SECONDS);
      assertEquals("train-42", a1.taskId());

      // Session A: handler throws. Must surface as an RpcError without removing the instance.
      try {
        sa.<Heartbeat, Assignment>call(
                AgentSessionDispatcher.SERVICE_NAME,
                "chaosFail",
                new Heartbeat("agent-A", List.of("gpu"), 0.5d),
                Assignment.class)
            .get(15, TimeUnit.SECONDS);
        fail("chaosFail should have thrown");
      } catch (ExecutionException ee) {
        Throwable cause = ee.getCause();
        assertTrue(
            cause instanceof RpcError,
            "expected RpcError, got " + (cause == null ? "null" : cause.getClass()));
      }

      // Session A: next call must still succeed — the instance is NOT zombied by the throw.
      // Prior `register` left capabilities=[gpu], so register again with gpu still yields
      // "train-42"; the key observation is that the call completes at all.
      Assignment a2 =
          sa.<Heartbeat, Assignment>call(
                  AgentSessionDispatcher.SERVICE_NAME,
                  "register",
                  new Heartbeat("agent-A", List.of("gpu"), 0.5d),
                  Assignment.class)
              .get(15, TimeUnit.SECONDS);
      assertEquals("train-42", a2.taskId());

      // Session B: must be completely untouched. Uses cpu only → taskId="idle", which would
      // differ from session A's state if cross-session leakage had occurred.
      Assignment b1 =
          sb.<Heartbeat, Assignment>call(
                  AgentSessionDispatcher.SERVICE_NAME,
                  "register",
                  new Heartbeat("agent-B", List.of("cpu"), 0.1d),
                  Assignment.class)
              .get(15, TimeUnit.SECONDS);
      assertEquals("idle", b1.taskId());

      // Snapshot: both sessions still alive on the server.
      var snap = f.server.debugConnectionSnapshot();
      assertEquals(1, snap.size());
      long connId = snap.keySet().iterator().next();
      assertEquals(2, snap.get(connId).activeSessionCount());
    }
  }

  // ── 3. Graveyard enforcement under out-of-order arrival ────────────────

  @Test
  void graveyardRejectsOlderSessionIdAfterLastOpenedAdvanced() throws Exception {
    try (Fixture f = Fixture.start()) {
      IrohConnection conn = f.client.connect(f.serverAddr).get(15, TimeUnit.SECONDS);

      // Drive session 2 FIRST via forTest (bypassing the monotonic allocator).
      try (ClientSession s2 = ClientSession.forTest(f.client, conn, 2)) {
        Assignment r2 =
            s2.<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "register",
                    new Heartbeat("agent-two", List.of("gpu"), 0.5d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);
        assertEquals("train-42", r2.taskId());

        // Now try session 1 — must be rejected with NOT_FOUND per §7.5 graveyard logic:
        // sessionId=1 is <= lastOpenedSessionId=2 and not in the active set.
        try (ClientSession s1 = ClientSession.forTest(f.client, conn, 1)) {
          try {
            s1.<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "register",
                    new Heartbeat("agent-one", List.of("cpu"), 0.1d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);
            fail("expected NOT_FOUND for session 1 (graveyard)");
          } catch (ExecutionException ee) {
            RpcError err = expectRpcError(ee);
            assertEquals(StatusCode.NOT_FOUND, err.code());
          }
        }

        // The graveyard rejection must not have corrupted session 2. A follow-up call on
        // session 2 still succeeds.
        Assignment r2b =
            s2.<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "heartbeat",
                    new Heartbeat("agent-two", List.of("gpu"), 0.5d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);
        assertEquals("continue", r2b.taskId());

        // Snapshot: still exactly one active session (sessionId=2) and lastOpened=2.
        var snap = f.server.debugConnectionSnapshot();
        long connId = snap.keySet().iterator().next();
        assertEquals(1, snap.get(connId).activeSessionCount());
        assertEquals(2, snap.get(connId).lastOpenedSessionId());
      }
    }
  }

  // ── 4a. Session cap, sequenced ─────────────────────────────────────────

  @Test
  void sessionCapEnforcedWhenOpenSessionCallsAreSerialised() throws Exception {
    final int CAP = 4;
    final int EXTRA = 3;
    try (Fixture f = Fixture.startWithMaxSessions(CAP)) {
      List<Assignment> fulfilled = new ArrayList<>();
      List<StatusCode> rejectedCodes = new ArrayList<>();
      for (int i = 0; i < CAP + EXTRA; i++) {
        ClientSession s = f.client.openSession(f.serverAddr).get(15, TimeUnit.SECONDS);
        try {
          Assignment a =
              s.<Heartbeat, Assignment>call(
                      AgentSessionDispatcher.SERVICE_NAME,
                      "register",
                      new Heartbeat("seq-" + i, List.of("cpu"), 0.1d),
                      Assignment.class)
                  .get(15, TimeUnit.SECONDS);
          fulfilled.add(a);
        } catch (ExecutionException ee) {
          RpcError err = expectRpcError(ee);
          rejectedCodes.add(err.code());
        }
      }

      assertEquals(CAP, fulfilled.size(), "expected exactly CAP fulfilled calls");
      assertEquals(EXTRA, rejectedCodes.size(), "expected exactly EXTRA rejections");
      for (StatusCode code : rejectedCodes) {
        assertEquals(StatusCode.RESOURCE_EXHAUSTED, code);
      }
      var snap = f.server.debugConnectionSnapshot();
      long connId = snap.keySet().iterator().next();
      assertEquals(CAP, snap.get(connId).activeSessionCount());
    }
  }

  // ── 4b. Session cap, concurrent burst ──────────────────────────────────

  @Test
  void sessionCapUnderConcurrentBurstNeverExceedsCapAndFailsWithNotFoundOrResourceExhausted()
      throws Exception {
    final int CAP = 4;
    final int BURST = 12;
    try (Fixture f = Fixture.startWithMaxSessions(CAP)) {
      // Kick off BURST concurrent openSession + call pairs. Each runs on the same
      // virtual-thread call executor so the QUIC stream open order is non-deterministic.
      List<CompletableFuture<Assignment>> futures = new ArrayList<>();
      for (int i = 0; i < BURST; i++) {
        final int idx = i;
        futures.add(
            f.client
                .openSession(f.serverAddr)
                .thenCompose(
                    session ->
                        session
                            .<Heartbeat, Assignment>call(
                                AgentSessionDispatcher.SERVICE_NAME,
                                "register",
                                new Heartbeat("burst-" + idx, List.of("cpu"), 0.1d),
                                Assignment.class)
                            .whenComplete((r, t) -> session.close())));
      }

      int fulfilled = 0;
      List<StatusCode> rejectedCodes = new ArrayList<>();
      for (CompletableFuture<Assignment> fut : futures) {
        try {
          fut.get(30, TimeUnit.SECONDS);
          fulfilled++;
        } catch (ExecutionException ee) {
          RpcError err = expectRpcError(ee);
          rejectedCodes.add(err.code());
        }
      }
      assertTrue(fulfilled <= CAP, "expected at most CAP successes; got " + fulfilled);
      assertEquals(BURST, fulfilled + rejectedCodes.size());
      for (StatusCode code : rejectedCodes) {
        assertTrue(
            code == StatusCode.NOT_FOUND || code == StatusCode.RESOURCE_EXHAUSTED,
            "rejection code should be NOT_FOUND or RESOURCE_EXHAUSTED; got " + code);
      }
      var snap = f.server.debugConnectionSnapshot();
      long connId = snap.keySet().iterator().next();
      // Active session count matches fulfilled — no leak, no double-count.
      assertEquals(fulfilled, snap.get(connId).activeSessionCount());
    }
  }

  // ── 5. Cross-connection session id isolation ───────────────────────────

  @Test
  void sameSessionIdOnTwoDistinctConnectionsResolvesToDistinctInstances() throws Exception {
    // AsterClient caches connections per-peer, so we need TWO independent
    // AsterClient instances to get two distinct QUIC connections to the same
    // server. This mirrors the TS test which uses two separate iroh nodes.
    AsterClient client2 = null;
    try (Fixture f = Fixture.start()) {
      ForyCodec codec2 = new ForyCodec();
      Server.registerWireTypes(codec2);
      client2 = AsterClient.builder().codec(codec2).build().get(15, TimeUnit.SECONDS);

      IrohConnection connA = f.client.connect(f.serverAddr).get(15, TimeUnit.SECONDS);
      IrohConnection connB = client2.connect(f.serverAddr).get(15, TimeUnit.SECONDS);

      // Both use sessionId=1 via forTest — valid only because they're on distinct connections.
      try (ClientSession sA = ClientSession.forTest(f.client, connA, 1);
          ClientSession sB = ClientSession.forTest(client2, connB, 1)) {

        Assignment rA =
            sA.<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "register",
                    new Heartbeat("agent-A", List.of("gpu"), 0.5d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);
        Assignment rB =
            sB.<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "register",
                    new Heartbeat("agent-B", List.of("cpu"), 0.1d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);

        // Distinct capabilities → distinct assignments. If sessions collapsed onto one
        // instance (keyed by sessionId alone instead of (connectionId, sessionId)), the
        // later call would have overwritten capabilities and both responses would match.
        assertEquals("train-42", rA.taskId());
        assertEquals("idle", rB.taskId());

        // Snapshot: exactly TWO connections, each with one active session.
        var snap = f.server.debugConnectionSnapshot();
        assertEquals(2, snap.size());
        for (var state : snap.values()) {
          assertEquals(1, state.activeSessionCount());
          assertEquals(1, state.lastOpenedSessionId());
        }
      }
    } finally {
      if (client2 != null) {
        try {
          client2.close();
        } catch (Exception ignored) {
        }
      }
    }
  }

  // ─── Helpers ──────────────────────────────────────────────────────────────

  private static RpcError expectRpcError(ExecutionException ee) {
    Throwable cause = ee.getCause();
    if (cause instanceof RpcError rpc) {
      return rpc;
    }
    throw new AssertionError("expected RpcError, got " + cause, cause);
  }

  private static boolean waitFor(
      java.util.function.BooleanSupplier predicate, long timeoutMs, long stepMs)
      throws InterruptedException {
    long deadline = System.currentTimeMillis() + timeoutMs;
    while (System.currentTimeMillis() < deadline) {
      if (predicate.getAsBoolean()) return true;
      Thread.sleep(stepMs);
    }
    return predicate.getAsBoolean();
  }

  private static final class Fixture implements AutoCloseable {
    final AsterServer server;
    final AsterClient client;
    final NodeAddr serverAddr;

    private Fixture(AsterServer server, AsterClient client) {
      this.server = server;
      this.client = client;
      this.serverAddr = server.node().nodeAddr();
    }

    static Fixture start() throws Exception {
      return startWithMaxSessions(1024);
    }

    static Fixture startWithMaxSessions(int maxSessions) throws Exception {
      ForyCodec serverCodec = new ForyCodec();
      Server.registerWireTypes(serverCodec);
      ForyCodec clientCodec = new ForyCodec();
      Server.registerWireTypes(clientCodec);

      MissionControl missionControl = new MissionControl();

      AsterServer server =
          AsterServer.builder()
              .codec(serverCodec)
              .service(missionControl)
              .sessionService(AgentSession.class, AgentSession::new)
              .maxSessionsPerConnection(maxSessions)
              .build()
              .get(15, TimeUnit.SECONDS);

      AsterClient client =
          AsterClient.builder().codec(clientCodec).build().get(15, TimeUnit.SECONDS);

      return new Fixture(server, client);
    }

    @Override
    public void close() {
      try {
        client.close();
      } catch (Exception ignored) {
      }
      server.close();
    }
  }
}
