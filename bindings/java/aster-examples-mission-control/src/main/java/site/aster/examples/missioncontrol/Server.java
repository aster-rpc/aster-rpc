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
 * <p>Java port of {@code examples/python/mission_control/server.py}. Implements unary +
 * server-streaming methods only; client-streaming and bidi-streaming methods from the Python sample
 * are not yet wired (reactor read-side multi-frame support is open work).
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
    System.out.println("  node addr : " + server.node().nodeAddr());
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
    codec.fory().register(StatusRequest.class, StatusRequest.FORY_TAG);
    codec.fory().register(StatusResponse.class, StatusResponse.FORY_TAG);
    codec.fory().register(LogEntry.class, LogEntry.FORY_TAG);
    codec.fory().register(SubmitLogResult.class, SubmitLogResult.FORY_TAG);
    codec.fory().register(TailRequest.class, TailRequest.FORY_TAG);
    codec.fory().register(Heartbeat.class, Heartbeat.FORY_TAG);
    codec.fory().register(Assignment.class, Assignment.FORY_TAG);
    codec.fory().register(MetricPoint.class, MetricPoint.FORY_TAG);
    codec.fory().register(IngestResult.class, IngestResult.FORY_TAG);
    codec.fory().register(Command.class, Command.FORY_TAG);
    codec.fory().register(CommandResult.class, CommandResult.FORY_TAG);
  }

  private Server() {}
}
