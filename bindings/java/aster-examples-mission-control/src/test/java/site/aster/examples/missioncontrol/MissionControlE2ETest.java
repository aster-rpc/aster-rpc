package site.aster.examples.missioncontrol;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.Map;
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
import site.aster.examples.missioncontrol.types.IngestResult;
import site.aster.examples.missioncontrol.types.LogEntry;
import site.aster.examples.missioncontrol.types.MetricPoint;
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
@Timeout(value = 30)
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
      try (ClientSession session =
          f.client.openSession(f.serverAddr).orTimeout(15, TimeUnit.SECONDS).get()) {
        Assignment assignment =
            session
                .<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME, "register", hb, Assignment.class)
                .orTimeout(15, TimeUnit.SECONDS)
                .get();

        assertEquals("train-42", assignment.taskId());
        assertEquals("python train.py", assignment.command());
      }
    }
  }

  @Test
  void sessionScopedRegisterWithoutGpuFallsBack() throws Exception {
    try (Fixture f = Fixture.start()) {
      Heartbeat hb = new Heartbeat("agent-8", List.of("cpu"), 0.10d);
      try (ClientSession session =
          f.client.openSession(f.serverAddr).orTimeout(15, TimeUnit.SECONDS).get()) {
        Assignment assignment =
            session
                .<Heartbeat, Assignment>call(
                    AgentSessionDispatcher.SERVICE_NAME, "register", hb, Assignment.class)
                .orTimeout(15, TimeUnit.SECONDS)
                .get();

        assertEquals("idle", assignment.taskId());
        assertEquals("sleep 60", assignment.command());
      }
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

  @Test
  void clientStreamIngestMetricsRoundTrip() throws Exception {
    try (Fixture f = Fixture.start()) {
      List<MetricPoint> points =
          List.of(
              new MetricPoint("cpu.user", 0.42d, 1.0d, Map.of("host", "agent-1")),
              new MetricPoint("cpu.user", 0.48d, 2.0d, Map.of("host", "agent-1")),
              new MetricPoint("mem.rss", 1024.0d, 3.0d, Map.of("host", "agent-1")));

      IngestResult result =
          f.client
              .<MetricPoint, IngestResult>callClientStream(
                  f.serverAddr,
                  MissionControlDispatcher.SERVICE_NAME,
                  "ingestMetrics",
                  points,
                  IngestResult.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();

      assertEquals(3, result.accepted());
      assertEquals(0, result.dropped());

      List<MetricPoint> stored = f.missionControl.metricsSnapshot();
      assertEquals(3, stored.size());
      assertEquals("cpu.user", stored.get(0).name());
      assertEquals("mem.rss", stored.get(2).name());
      assertEquals(1024.0d, stored.get(2).value());
    }
  }

  @Test
  void bidiStreamRunCommandPingPong() throws Exception {
    try (Fixture f = Fixture.start()) {
      List<Command> cmds =
          List.of(new Command("ls -l"), new Command("echo hello"), new Command("uptime"));

      try (ClientSession session =
          f.client.openSession(f.serverAddr).orTimeout(15, TimeUnit.SECONDS).get()) {
        List<CommandResult> results =
            session
                .<Command, CommandResult>callBidiStream(
                    AgentSessionDispatcher.SERVICE_NAME, "runCommand", cmds, CommandResult.class)
                .orTimeout(15, TimeUnit.SECONDS)
                .get();

        assertEquals(3, results.size());
        assertEquals("ran: ls -l", results.get(0).stdout());
        assertEquals("ran: echo hello", results.get(1).stdout());
        assertEquals("ran: uptime", results.get(2).stdout());
        for (CommandResult r : results) {
          assertEquals(0, r.exitCode());
          assertEquals("", r.stderr());
        }
      }
    }
  }

  @Test
  void interleavedBidiRunCommandPingPong() throws Exception {
    try (Fixture f = Fixture.start();
        ClientSession session =
            f.client.openSession(f.serverAddr).orTimeout(15, TimeUnit.SECONDS).get()) {
      try (BidiCall<Command, CommandResult> call =
          session.<Command, CommandResult>openBidiStream(
              AgentSessionDispatcher.SERVICE_NAME, "runCommand", CommandResult.class)) {

        // True ping-pong: send one command, immediately receive its result, then send the next.
        // The buffered callBidiStream can't model this — it materializes all requests first.
        call.send(new Command("ls"));
        CommandResult r1 = call.recv();
        assertNotNull(r1);
        assertEquals("ran: ls", r1.stdout());

        call.send(new Command("date"));
        CommandResult r2 = call.recv();
        assertNotNull(r2);
        assertEquals("ran: date", r2.stdout());

        call.send(new Command("uptime"));
        CommandResult r3 = call.recv();
        assertNotNull(r3);
        assertEquals("ran: uptime", r3.stdout());

        call.complete();

        // Server's runCommand drains in.receive() until null, so after complete() the dispatcher
        // returns and the runtime sends the OK trailer. recv() should return null cleanly.
        CommandResult tail = call.recv();
        assertNull(tail, "expected end-of-stream after complete()");
      }
    }
  }

  @Test
  void bidiRunCommandCancellationPropagates() throws Exception {
    AgentSession.lastRunCommandExitReason = "";
    try (Fixture f = Fixture.start();
        ClientSession session =
            f.client.openSession(f.serverAddr).orTimeout(15, TimeUnit.SECONDS).get()) {
      try (BidiCall<Command, CommandResult> call =
          session.<Command, CommandResult>openBidiStream(
              AgentSessionDispatcher.SERVICE_NAME, "runCommand", CommandResult.class)) {

        call.send(new Command("ls"));
        CommandResult r1 = call.recv();
        assertNotNull(r1);
        assertEquals("ran: ls", r1.stdout());

        // Send a CANCEL frame instead of more commands. The reactor should observe it,
        // set the per-call cancelled flag, and close the request channel. The server's
        // runCommand loop sees in.receive() return null and the second isCancelled()
        // check inside the dispatcher records "CANCELLED" as the exit reason.
        call.cancel();

        // Drain any trailing responses + the trailer. Should hit end-of-stream quickly.
        CommandResult tail = call.recv();
        assertNull(tail, "expected end-of-stream after cancel()");
      }
    }

    assertEquals(
        "CANCELLED",
        AgentSession.lastRunCommandExitReason,
        "server-side runCommand should record CANCELLED exit reason after the client cancels");
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
