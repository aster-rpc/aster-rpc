package site.aster.examples.missioncontrol;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.client.AsterClient;
import site.aster.codec.ForyCodec;
import site.aster.examples.missioncontrol.types.Assignment;
import site.aster.examples.missioncontrol.types.Heartbeat;
import site.aster.examples.missioncontrol.types.LogEntry;
import site.aster.examples.missioncontrol.types.StatusRequest;
import site.aster.examples.missioncontrol.types.StatusResponse;
import site.aster.examples.missioncontrol.types.SubmitLogResult;
import site.aster.examples.missioncontrol.types.TailRequest;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * Java-to-Java end-to-end smoke test for the Mission Control sample. Brings up a real server with
 * both services registered, opens a client, exercises every implemented method (3 unary + 1
 * server-stream), and verifies responses.
 *
 * <p>This test is the milestone gate for "Java Mission Control server runs". It exercises:
 *
 * <ul>
 *   <li>Shared-scope service registration ({@code MissionControl})
 *   <li>Session-scoped service registration ({@code AgentSession}) with a per-peer factory
 *   <li>Three unary methods across two services
 *   <li>One server-streaming method, including the queue feed → frame delivery path
 *   <li>Plain-class wire types with collection fields ({@code Heartbeat.capabilities})
 * </ul>
 */
final class MissionControlE2ETest {

  @Test
  void unaryStatusRoundTrip() throws Exception {
    try (Fixture f = Fixture.start()) {
      StatusResponse resp =
          f.client
              .<StatusRequest, StatusResponse>call(
                  f.serverAddr,
                  MissionControlDispatcher.SERVICE_NAME,
                  "getStatus",
                  new StatusRequest("agent-1"),
                  StatusResponse.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();

      assertEquals("agent-1", resp.agentId());
      assertEquals("running", resp.status());
      assertTrue(resp.uptimeSecs() >= 0L);
    }
  }

  @Test
  void unarySubmitLogRoundTrip() throws Exception {
    try (Fixture f = Fixture.start()) {
      LogEntry entry = new LogEntry(1.0d, "info", "hello", "agent-1");
      SubmitLogResult result =
          f.client
              .<LogEntry, SubmitLogResult>call(
                  f.serverAddr,
                  MissionControlDispatcher.SERVICE_NAME,
                  "submitLog",
                  entry,
                  SubmitLogResult.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();

      assertTrue(result.accepted());
      assertEquals(1, f.missionControl.logQueue().size());
    }
  }

  @Test
  void sessionScopedRegisterReturnsAssignment() throws Exception {
    try (Fixture f = Fixture.start()) {
      Heartbeat hb = new Heartbeat("agent-7", List.of("gpu", "cuda12"), 0.42d);
      Assignment assignment =
          f.client
              .<Heartbeat, Assignment>call(
                  f.serverAddr,
                  AgentSessionDispatcher.SERVICE_NAME,
                  "register",
                  hb,
                  Assignment.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();

      assertEquals("train-42", assignment.taskId());
      assertEquals("python train.py", assignment.command());
    }
  }

  @Test
  void sessionScopedRegisterWithoutGpuFallsBack() throws Exception {
    try (Fixture f = Fixture.start()) {
      Heartbeat hb = new Heartbeat("agent-8", List.of("cpu"), 0.10d);
      Assignment assignment =
          f.client
              .<Heartbeat, Assignment>call(
                  f.serverAddr,
                  AgentSessionDispatcher.SERVICE_NAME,
                  "register",
                  hb,
                  Assignment.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();

      assertEquals("idle", assignment.taskId());
      assertEquals("sleep 60", assignment.command());
    }
  }

  @Test
  void serverStreamTailLogsDeliversBufferedEntries() throws Exception {
    try (Fixture f = Fixture.start()) {
      // Pre-seed the queue so tailLogs has something to drain. The server method exits as soon as
      // the queue is idle for ~250ms, which is the right shape for a deterministic test.
      for (int i = 0; i < 4; i++) {
        f.missionControl.logQueue().offer(new LogEntry(i, "info", "line-" + i, "agent-1"));
      }

      List<LogEntry> entries =
          f.client
              .<TailRequest, LogEntry>callServerStream(
                  f.serverAddr,
                  MissionControlDispatcher.SERVICE_NAME,
                  "tailLogs",
                  new TailRequest("agent-1", "info"),
                  LogEntry.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();

      assertEquals(4, entries.size());
      for (int i = 0; i < entries.size(); i++) {
        assertEquals("line-" + i, entries.get(i).message());
        assertEquals("agent-1", entries.get(i).agentId());
      }
    }
  }

  @Test
  void serverStreamTailLogsFiltersByLevel() throws Exception {
    try (Fixture f = Fixture.start()) {
      f.missionControl.logQueue().offer(new LogEntry(0, "debug", "noise", "agent-1"));
      f.missionControl.logQueue().offer(new LogEntry(1, "info", "ok", "agent-1"));
      f.missionControl.logQueue().offer(new LogEntry(2, "error", "boom", "agent-1"));

      List<LogEntry> entries =
          f.client
              .<TailRequest, LogEntry>callServerStream(
                  f.serverAddr,
                  MissionControlDispatcher.SERVICE_NAME,
                  "tailLogs",
                  new TailRequest("agent-1", "warn"),
                  LogEntry.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();

      assertEquals(1, entries.size());
      assertEquals("boom", entries.get(0).message());
      assertEquals("error", entries.get(0).level());
    }
  }

  // ─── Fixture ──────────────────────────────────────────────────────────────

  private static final class Fixture implements AutoCloseable {
    final AsterServer server;
    final AsterClient client;
    final NodeAddr serverAddr;
    final MissionControl missionControl;

    private Fixture(AsterServer server, AsterClient client, MissionControl missionControl) {
      this.server = server;
      this.client = client;
      this.serverAddr = server.node().nodeAddr();
      this.missionControl = missionControl;
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

      assertNotNull(server.node().nodeAddr(), "server should expose a node addr");
      return new Fixture(server, client, missionControl);
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
