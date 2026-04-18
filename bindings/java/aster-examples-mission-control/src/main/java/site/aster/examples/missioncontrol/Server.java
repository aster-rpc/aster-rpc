package site.aster.examples.missioncontrol;

import java.util.concurrent.TimeUnit;
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
import site.aster.server.AsterServer;

/**
 * Bootstraps a Java Mission Control server: registers both the shared {@link MissionControl} and
 * session-scoped {@link AgentSession} services, prints the node address, and parks until
 * interrupted (Ctrl-C).
 *
 * <p>Java port of {@code examples/python/mission_control/server.py}. Mirrors all four Python RPC
 * patterns: unary, server-stream, client-stream, and bidi-stream.
 *
 * <p>Usage: {@code mvn -P fast -pl aster-examples-mission-control exec:java
 * -Dexec.mainClass=site.aster.examples.missioncontrol.Server}
 */
public final class Server {

  public static void main(String[] args) throws Exception {
    ForyCodec codec = new ForyCodec();
    registerWireTypes(codec);

    MissionControl missionControl = new MissionControl();

    AsterServer server =
        AsterServer.builder()
            .codec(codec)
            .service(missionControl)
            .sessionService(AgentSession.class, AgentSession::new)
            .build()
            .get(15, TimeUnit.SECONDS);

    System.out.println("Mission Control server started");
    System.out.println("  node id   : " + server.nodeId());
    site.aster.node.NodeAddr addr = server.node().nodeAddr();
    System.out.println("  node addr : " + addr);
    // Emit the aster1… ticket on its own line — matrix harness and cross-language clients
    // consume this verbatim. printed LAST among address lines so that downstream `grep
    // -oE 'aster1[A-Za-z0-9]*'` parses unambiguously.
    System.out.println(addr.toTicket());
    System.out.println("  services  :");
    server.manifest().forEach(d -> System.out.println("    - " + d.name() + " v" + d.version()));
    System.out.println("Ctrl-C to stop.");

    Runtime.getRuntime()
        .addShutdownHook(
            new Thread(
                () -> {
                  System.out.println("Stopping Mission Control server…");
                  server.close();
                }));

    Thread.currentThread().join();
  }

  /**
   * Pre-register every wire type on the user codec. The runtime auto-registers framework wire types
   * (StreamHeader / CallHeader / RpcStatus) but user payload types are the application's
   * responsibility.
   */
  public static void registerWireTypes(ForyCodec codec) {
    var fory = codec.fory();
    site.aster.codec.ForyTags.register(fory, StatusRequest.class, StatusRequest.FORY_TAG);
    site.aster.codec.ForyTags.register(fory, StatusResponse.class, StatusResponse.FORY_TAG);
    site.aster.codec.ForyTags.register(fory, LogEntry.class, LogEntry.FORY_TAG);
    site.aster.codec.ForyTags.register(fory, SubmitLogResult.class, SubmitLogResult.FORY_TAG);
    site.aster.codec.ForyTags.register(fory, TailRequest.class, TailRequest.FORY_TAG);
    site.aster.codec.ForyTags.register(fory, Heartbeat.class, Heartbeat.FORY_TAG);
    site.aster.codec.ForyTags.register(fory, Assignment.class, Assignment.FORY_TAG);
    site.aster.codec.ForyTags.register(fory, MetricPoint.class, MetricPoint.FORY_TAG);
    site.aster.codec.ForyTags.register(fory, IngestResult.class, IngestResult.FORY_TAG);
    site.aster.codec.ForyTags.register(fory, Command.class, Command.FORY_TAG);
    site.aster.codec.ForyTags.register(fory, CommandResult.class, CommandResult.FORY_TAG);
  }

  private Server() {}
}
