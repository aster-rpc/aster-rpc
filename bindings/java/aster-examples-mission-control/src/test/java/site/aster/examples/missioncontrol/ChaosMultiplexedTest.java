package site.aster.examples.missioncontrol;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import java.util.List;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import site.aster.client.AsterClient;
import site.aster.client.ClientSession;
import site.aster.codec.ForyCodec;
import site.aster.examples.missioncontrol.types.Assignment;
import site.aster.examples.missioncontrol.types.Heartbeat;
import site.aster.interceptors.RpcError;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * The multiplexed-streams binding layer (Java port of {@code
 * tests/typescript/integration/chaos-multiplexed.test.ts}): a handler exception on session A
 * surfaces as an {@link RpcError}, does not poison A's Java instance and does not leak into session
 * B.
 *
 * <p>The session rules are Rust's and are tested once there, in {@code
 * aster/tests/it/rpc_sessions.rs}: reaping on connection close ({@code
 * closing_a_connection_reaps_its_sessions_and_only_its_sessions}), the graveyard ({@code
 * a_session_id_at_or_below_last_opened_that_is_not_active_is_not_found}), the per-connection cap
 * ({@code a_new_session_over_the_cap_is_resource_exhausted_without_bumping_last_opened}) and
 * per-connection session ids ({@code a_second_connection_cannot_reach_another_connections_session},
 * {@code sessions_are_per_connection_and_reaped_on_close_over_iroh}).
 */
@Timeout(value = 60)
final class ChaosMultiplexedTest {

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
        // swallow-ok: best-effort teardown; the resource may already be closed
      }
      server.close();
    }
  }
}
