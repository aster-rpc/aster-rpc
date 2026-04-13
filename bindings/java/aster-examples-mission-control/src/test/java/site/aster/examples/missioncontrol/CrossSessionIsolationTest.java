package site.aster.examples.missioncontrol;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNotSame;
import static org.junit.jupiter.api.Assertions.assertSame;
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
import site.aster.client.BidiCall;
import site.aster.client.ClientSession;
import site.aster.codec.ForyCodec;
import site.aster.examples.missioncontrol.types.Assignment;
import site.aster.examples.missioncontrol.types.Command;
import site.aster.examples.missioncontrol.types.CommandResult;
import site.aster.examples.missioncontrol.types.Heartbeat;
import site.aster.examples.missioncontrol.types.StatusRequest;
import site.aster.examples.missioncontrol.types.StatusResponse;
import site.aster.handle.IrohConnection;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * Cross-session isolation tests for the multiplexed-streams server (spec §6 / §7.5).
 *
 * <p>Verifies that unexpected client behaviour (reordered streams, repeated sessionIds, sessionIds
 * past the cap, scope mismatches) doesn't muddle session state within a connection. These tests
 * deliberately bypass {@link AsterClient#openSession}'s monotonic allocator via {@link
 * ClientSession#forTest} so they can drive the server's lookup-or-create logic with adversarial
 * inputs.
 */
@Timeout(value = 30)
final class CrossSessionIsolationTest {

  /**
   * Two sessions on the same connection with different sessionIds yield two independent {@link
   * AgentSession} instances, so per-session state from one does not leak into the other.
   */
  @Test
  void twoSessionsOnSameConnectionAreIndependent() throws Exception {
    try (Fixture f = Fixture.start()) {
      IrohConnection conn = f.client.connect(f.serverAddr).get(15, TimeUnit.SECONDS);

      try (ClientSession session1 = ClientSession.forTest(f.client, conn, 1);
          ClientSession session2 = ClientSession.forTest(f.client, conn, 2)) {

        Assignment a1 =
            session1
                .<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "register",
                    new Heartbeat("agent-with-gpu", List.of("gpu", "cuda12"), 0.5d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);

        Assignment a2 =
            session2
                .<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "register",
                    new Heartbeat("agent-cpu-only", List.of("cpu"), 0.1d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);

        // Session1 saw the GPU heartbeat, session2 saw the CPU heartbeat — distinct instances
        // produce distinct assignments. If the server collapsed them onto one instance, the
        // second register() would have overwritten capabilities and the responses wouldn't
        // diverge this cleanly.
        assertEquals("train-42", a1.taskId());
        assertEquals("idle", a2.taskId());
      }
    }
  }

  /**
   * Re-using a sessionId that's already been opened (and is currently in the active map) returns
   * the same instance — important so multiple calls on the same session compose. Skipping
   * sessionIds is fine and bumps the graveyard counter to the highest seen.
   */
  @Test
  void skippingSessionIdsAndReuseBehaveCorrectly() throws Exception {
    try (Fixture f = Fixture.start()) {
      IrohConnection conn = f.client.connect(f.serverAddr).get(15, TimeUnit.SECONDS);

      // Use sparse, non-monotonic-from-1 sessionIds — server should accept all of them as long
      // as each is greater than the prior lastOpenedSessionId at the time of arrival.
      try (ClientSession s5 = ClientSession.forTest(f.client, conn, 5);
          ClientSession s17 = ClientSession.forTest(f.client, conn, 17);
          ClientSession s5again = ClientSession.forTest(f.client, conn, 5)) {

        Assignment a5 =
            s5.<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "register",
                    new Heartbeat("agent-5", List.of("gpu"), 0.5d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);
        assertEquals("train-42", a5.taskId());

        Assignment a17 =
            s17.<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "register",
                    new Heartbeat("agent-17", List.of("gpu"), 0.5d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);
        assertEquals("train-42", a17.taskId());

        // Re-using sessionId=5 (still in the active map) should hit the EXISTING instance,
        // not the graveyard — because we never released it. heartbeat()'s response is the same
        // regardless of instance, but the call must SUCCEED rather than NOT_FOUND.
        Assignment a5b =
            s5again
                .<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "heartbeat",
                    new Heartbeat("agent-5-again", List.of("gpu"), 0.6d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);
        assertEquals("continue", a5b.taskId());
      }
    }
  }

  /**
   * A SHARED-scoped service called with a non-zero sessionId is a scope mismatch and must surface
   * as {@link StatusCode#FAILED_PRECONDITION}.
   */
  @Test
  void sharedServiceWithSessionIdIsScopeMismatch() throws Exception {
    try (Fixture f = Fixture.start()) {
      IrohConnection conn = f.client.connect(f.serverAddr).get(15, TimeUnit.SECONDS);

      try (ClientSession bogus = ClientSession.forTest(f.client, conn, 99)) {
        try {
          bogus
              .<StatusRequest, StatusResponse>call(
                  MissionControlDispatcher.SERVICE_NAME,
                  "getStatus",
                  new StatusRequest("agent-1"),
                  StatusResponse.class)
              .get(15, TimeUnit.SECONDS);
          fail("expected FAILED_PRECONDITION for SHARED service called with non-zero sessionId");
        } catch (ExecutionException e) {
          RpcError err = unwrapRpc(e);
          assertEquals(StatusCode.FAILED_PRECONDITION, err.code());
          assertTrue(err.rpcMessage().contains("SHARED"));
        }
      }
    }
  }

  /**
   * A SESSION-scoped service called with sessionId=0 (the SHARED slot) is a scope mismatch and must
   * surface as {@link StatusCode#FAILED_PRECONDITION}.
   */
  @Test
  void sessionServiceWithoutSessionIdIsScopeMismatch() throws Exception {
    try (Fixture f = Fixture.start()) {
      try {
        // The default AsterClient.call uses sessionId=0 for SHARED routing; the AgentSession
        // service is SESSION-scoped, so this is the scope-mismatch failure path.
        f.client
            .<Heartbeat, Assignment>call(
                f.serverAddr,
                AgentSessionDispatcher.SERVICE_NAME,
                "register",
                new Heartbeat("agent-x", List.of("cpu"), 0.1d),
                Assignment.class)
            .get(15, TimeUnit.SECONDS);
        fail("expected FAILED_PRECONDITION for SESSION service called without sessionId");
      } catch (ExecutionException e) {
        RpcError err = unwrapRpc(e);
        assertEquals(StatusCode.FAILED_PRECONDITION, err.code());
        assertTrue(err.rpcMessage().contains("SESSION"));
      }
    }
  }

  /**
   * Exceeding {@code maxSessionsPerConnection} returns {@link StatusCode#RESOURCE_EXHAUSTED}. Spec
   * §7.5: the cap counts active sessions only and the graveyard counter is NOT bumped on rejection.
   */
  @Test
  void maxSessionsPerConnectionEnforcesCap() throws Exception {
    try (Fixture f = Fixture.startWithMaxSessions(2)) {
      IrohConnection conn = f.client.connect(f.serverAddr).get(15, TimeUnit.SECONDS);

      try (ClientSession s1 = ClientSession.forTest(f.client, conn, 1);
          ClientSession s2 = ClientSession.forTest(f.client, conn, 2);
          ClientSession s3 = ClientSession.forTest(f.client, conn, 3)) {

        // First two sessions fill the cap.
        Assignment a1 =
            s1.<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "register",
                    new Heartbeat("agent-1", List.of("gpu"), 0.1d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);
        Assignment a2 =
            s2.<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME,
                    "register",
                    new Heartbeat("agent-2", List.of("cpu"), 0.1d),
                    Assignment.class)
                .get(15, TimeUnit.SECONDS);
        assertNotNull(a1);
        assertNotNull(a2);

        // Third session exceeds the cap → RESOURCE_EXHAUSTED.
        try {
          s3.<Heartbeat, Assignment>call(
                  AgentSessionDispatcher.SERVICE_NAME,
                  "register",
                  new Heartbeat("agent-3", List.of("gpu"), 0.1d),
                  Assignment.class)
              .get(15, TimeUnit.SECONDS);
          fail("expected RESOURCE_EXHAUSTED for session beyond max_sessions_per_connection");
        } catch (ExecutionException e) {
          RpcError err = unwrapRpc(e);
          assertEquals(StatusCode.RESOURCE_EXHAUSTED, err.code());
          assertTrue(err.rpcMessage().contains("max_sessions_per_connection"));
        }
      }
    }
  }

  /**
   * Sanity check that {@link ClientSession#forTest} returns the same logical handle when called
   * with the same sessionId — the server is what enforces session identity, not the client. This is
   * mostly here to document that a client that re-creates its session handles after a restart could
   * still address an existing server-side session by sessionId, as long as the connection is still
   * alive.
   */
  @Test
  void clientSessionEqualityIsByReferenceNotSessionId() throws Exception {
    try (Fixture f = Fixture.start()) {
      IrohConnection conn = f.client.connect(f.serverAddr).get(15, TimeUnit.SECONDS);
      ClientSession a = ClientSession.forTest(f.client, conn, 42);
      ClientSession b = ClientSession.forTest(f.client, conn, 42);
      assertNotSame(a, b, "ClientSession is not value-typed; two handles for the same id are OK");
      assertEquals(a.sessionId(), b.sessionId());
      assertSame(a.connection(), b.connection());
    }
  }

  /**
   * Spec §4.4 regression: a long-running streaming call on a session must not starve concurrent
   * unary calls on the same session. With default {@code session_pool_size=1}, a naive
   * implementation would route the streaming call through the single-slot session pool, and
   * subsequent unary calls would queue on {@code POOL_FULL} until the streaming call drains. Spec
   * §3 line 65 ("streaming substreams don't count against any pool") requires the client to open
   * streaming calls on a dedicated substream that bypasses the pool entirely. This test is the
   * regression gate for that invariant on the Java binding.
   */
  @Test
  void streamingCallDoesNotStarveConcurrentUnaryOnSameSession() throws Exception {
    try (Fixture f = Fixture.start();
        ClientSession session =
            f.client.openSession(f.serverAddr).orTimeout(15, TimeUnit.SECONDS).get()) {

      // Open a bidi streaming call on the same session. Hold it open
      // (no complete()) so its substream stays alive for the whole
      // unary fan-out below.
      try (BidiCall<Command, CommandResult> held =
          session.<Command, CommandResult>openBidiStream(
              AgentSessionDispatcher.SERVICE_NAME, "runCommand", CommandResult.class)) {
        held.send(new Command("ls"));
        CommandResult first = held.recv();
        assertNotNull(first);

        // Fire 5 concurrent unary `register` calls on the same session.
        // Pre-fix these would queue on POOL_FULL because the bidi holds
        // the single pool slot; the test wait budget would expire and
        // the assertion below would fail.
        List<CompletableFuture<Assignment>> futures = new ArrayList<>();
        for (int i = 0; i < 5; i++) {
          futures.add(
              session.<Heartbeat, Assignment>call(
                  AgentSessionDispatcher.SERVICE_NAME,
                  "register",
                  new Heartbeat("agent-" + i, List.of("cpu"), 0.1d),
                  Assignment.class));
        }
        CompletableFuture.allOf(futures.toArray(new CompletableFuture<?>[0]))
            .get(5, TimeUnit.SECONDS);
        for (CompletableFuture<Assignment> fut : futures) {
          assertNotNull(fut.get());
        }

        // Drain the held bidi cleanly so the fixture can shut down.
        held.complete();
        CommandResult tail = held.recv();
        assertTrue(tail == null || tail.stdout() != null);
      }
    }
  }

  // ─── Helpers ──────────────────────────────────────────────────────────────

  private static RpcError unwrapRpc(ExecutionException e) {
    Throwable cause = e.getCause();
    if (cause instanceof RpcError rpc) {
      return rpc;
    }
    throw new AssertionError("expected RpcError, got " + cause, cause);
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
