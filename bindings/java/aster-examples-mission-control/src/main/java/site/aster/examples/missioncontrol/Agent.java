package site.aster.examples.missioncontrol;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import site.aster.client.AsterClient;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.examples.missioncontrol.types.IngestResult;
import site.aster.examples.missioncontrol.types.LogEntry;
import site.aster.examples.missioncontrol.types.MetricPoint;
import site.aster.examples.missioncontrol.types.StatusRequest;
import site.aster.examples.missioncontrol.types.StatusResponse;
import site.aster.examples.missioncontrol.types.SubmitLogResult;
import site.aster.node.NodeAddr;

/**
 * Mission Control agent: connects with an {@link AsterClient} and calls {@code getStatus}, {@code
 * submitLog} and {@code ingestMetrics} (Python's {@code agent.py}, Rust's {@code bin/agent.rs},
 * Go's {@code agent}).
 *
 * <p>Configuration comes from {@link AsterConfig#fromEnv()}. Against the auth server set this
 * agent's key and credential ({@code ASTER_SECRET_KEY_FILE}, {@code ASTER_ENROLLMENT_CREDENTIAL})
 * and the same {@code ASTER_ROOT_PUBKEY}, {@code ASTER_ENROLLMENT_AUDIENCE} and {@code
 * ASTER_ENROLLMENT_FLEET_NAMESPACE} as the server: the client enrolls with the server before its
 * first call, and the roles in the credential decide which calls succeed. The address is the first
 * argument or {@code ASTER_ENDPOINT_ADDR}.
 */
public final class Agent {

  private Agent() {}

  public static void main(String[] args) throws Exception {
    AsterConfig config = AsterConfig.fromEnv();
    String address = args.length > 0 ? args[0] : config.endpointAddr();
    if (address == null || address.isBlank()) {
      System.err.println("usage: Agent <server address> (or ASTER_ENDPOINT_ADDR)");
      System.exit(2);
    }
    ForyCodec codec = new ForyCodec();
    Server.registerWireTypes(codec);
    NodeAddr server = NodeAddr.fromTicket(address);
    try (AsterClient client =
        AsterClient.builder().config(config).codec(codec).build().get(60, TimeUnit.SECONDS)) {
      try {
        StatusResponse status =
            client
                .call(
                    server,
                    "MissionControl",
                    "getStatus",
                    new StatusRequest("java-agent-1"),
                    StatusResponse.class)
                .get(30, TimeUnit.SECONDS);
        System.out.println("Status: " + status);
      } catch (ExecutionException e) {
        System.out.println("getStatus refused: " + e.getCause().getMessage());
      }

      double now = System.currentTimeMillis() / 1000.0;
      client
          .call(
              server,
              "MissionControl",
              "submitLog",
              new LogEntry(now, "info", "agent started", "java-agent-1"),
              SubmitLogResult.class)
          .get(30, TimeUnit.SECONDS);
      System.out.println("Log submitted");

      List<MetricPoint> points = new ArrayList<>();
      for (int i = 0; i < 100; i++) {
        points.add(new MetricPoint("cpu.usage", i % 100, now, Map.of()));
      }
      try {
        IngestResult result =
            client
                .callClientStream(
                    server, "MissionControl", "ingestMetrics", points, IngestResult.class)
                .get(30, TimeUnit.SECONDS);
        System.out.println("Metrics accepted: " + result.accepted());
      } catch (ExecutionException e) {
        System.out.println("ingestMetrics refused: " + e.getCause().getMessage());
      }
    }
  }
}
