package site.aster.examples.missioncontrol;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.Map;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import site.aster.client.AsterClient;
import site.aster.codec.ForyCodec;
import site.aster.examples.missioncontrol.types.LogEntry;
import site.aster.examples.missioncontrol.types.StatusRequest;
import site.aster.examples.missioncontrol.types.StatusResponse;
import site.aster.examples.missioncontrol.types.SubmitLogResult;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * End-to-end test for the auth-mode Mission Control server. Runs a real QUIC roundtrip with the
 * capability interceptor wired into the chain; roles are injected via {@link
 * AsterServer#setPeerAttributes(String, Map)} rather than a real admission credential pipeline (the
 * client-side metadata surface arrives in Phase 3).
 *
 * <p>Mirrors the Python {@code tests/integration/mission_control/test_guide.py} Chapter-5 suite.
 */
@Timeout(value = 30)
final class MissionControlAuthE2ETest {

  @Test
  void publicMethodAllowsCallerWithoutRoles() throws Exception {
    try (AuthFixture f = AuthFixture.start()) {
      SubmitLogResult result =
          f.client
              .<LogEntry, SubmitLogResult>call(
                  f.serverAddr,
                  "MissionControl",
                  "submitLog",
                  new LogEntry(1.0d, "info", "hello", "agent-1"),
                  SubmitLogResult.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();
      assertTrue(result.accepted());
    }
  }

  @Test
  void getStatusPassesWithOpsStatus() throws Exception {
    try (AuthFixture f = AuthFixture.start()) {
      f.grant(Role.STATUS);
      StatusResponse resp =
          f.client
              .<StatusRequest, StatusResponse>call(
                  f.serverAddr,
                  "MissionControl",
                  "getStatus",
                  new StatusRequest("agent-7"),
                  StatusResponse.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();
      assertEquals("agent-7", resp.agentId());
      assertEquals("running", resp.status());
    }
  }

  @Test
  void getStatusDeniedWithoutRole() throws Exception {
    try (AuthFixture f = AuthFixture.start()) {
      ExecutionException ex =
          assertDenies(
              () ->
                  f.client
                      .<StatusRequest, StatusResponse>call(
                          f.serverAddr,
                          "MissionControl",
                          "getStatus",
                          new StatusRequest("agent-7"),
                          StatusResponse.class)
                      .orTimeout(15, TimeUnit.SECONDS)
                      .get());
      assertSame(RpcError.class, ex.getCause().getClass());
      assertEquals(StatusCode.PERMISSION_DENIED, ((RpcError) ex.getCause()).code());
    }
  }

  @Test
  void getStatusDeniedWithWrongRole() throws Exception {
    try (AuthFixture f = AuthFixture.start()) {
      f.grant(Role.LOGS);
      ExecutionException ex =
          assertDenies(
              () ->
                  f.client
                      .<StatusRequest, StatusResponse>call(
                          f.serverAddr,
                          "MissionControl",
                          "getStatus",
                          new StatusRequest("agent-7"),
                          StatusResponse.class)
                      .orTimeout(15, TimeUnit.SECONDS)
                      .get());
      assertEquals(StatusCode.PERMISSION_DENIED, ((RpcError) ex.getCause()).code());
    }
  }

  @Test
  void anyOfAcceptsEitherListedRole() throws Exception {
    // tailLogs requires any_of(LOGS, ADMIN). Call w/ ADMIN only; response stream drains quickly
    // because no logs have been submitted and the handler exits on idle timeout.
    try (AuthFixture f = AuthFixture.start()) {
      f.grant(Role.ADMIN);
      java.util.List<LogEntry> entries =
          f.client
              .<site.aster.examples.missioncontrol.types.TailRequest, LogEntry>callServerStream(
                  f.serverAddr,
                  "MissionControl",
                  "tailLogs",
                  new site.aster.examples.missioncontrol.types.TailRequest("", "debug"),
                  LogEntry.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();
      assertNotNull(entries);
    }
  }

  private static ExecutionException assertDenies(ThrowingRunnable call) {
    return org.junit.jupiter.api.Assertions.assertThrows(ExecutionException.class, call::run);
  }

  @FunctionalInterface
  private interface ThrowingRunnable {
    void run() throws Exception;
  }

  // ─── Fixture ──────────────────────────────────────────────────────────────

  private static final class AuthFixture implements AutoCloseable {
    final AsterServer server;
    final AsterClient client;
    final NodeAddr serverAddr;
    final MissionControl missionControl;
    final String clientNodeId;

    private AuthFixture(
        AsterServer server, AsterClient client, MissionControl missionControl, String clientId) {
      this.server = server;
      this.client = client;
      this.serverAddr = server.node().nodeAddr();
      this.missionControl = missionControl;
      this.clientNodeId = clientId;
    }

    static AuthFixture start() throws Exception {
      ForyCodec serverCodec = new ForyCodec();
      Server.registerWireTypes(serverCodec);
      ForyCodec clientCodec = new ForyCodec();
      Server.registerWireTypes(clientCodec);

      MissionControl missionControl = new MissionControl();
      AsterServer server =
          ServerAuth.buildWithAuth(serverCodec, missionControl, AgentSession::new)
              .get(15, TimeUnit.SECONDS);
      AsterClient client =
          AsterClient.builder().codec(clientCodec).build().get(15, TimeUnit.SECONDS);
      assertNotNull(server.node().nodeAddr(), "server should expose a node addr");
      return new AuthFixture(server, client, missionControl, client.nodeId());
    }

    /**
     * Grant the single role to the test's client peer. Simulates admission having validated a
     * credential that carries {@code aster.role=<role>}.
     */
    void grant(String role) {
      server.setPeerAttributes(clientNodeId, Map.of("aster.role", role));
    }

    @Override
    public void close() {
      try {
        client.close();
      } catch (Exception ignored) {
        // best-effort
      }
      server.close();
    }
  }
}
