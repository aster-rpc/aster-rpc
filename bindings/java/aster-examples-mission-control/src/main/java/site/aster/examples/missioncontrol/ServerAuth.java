package site.aster.examples.missioncontrol;

import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.examples.missioncontrol.auth.AgentSessionAuthDispatcher;
import site.aster.examples.missioncontrol.auth.MetadataRoleInterceptor;
import site.aster.examples.missioncontrol.auth.MissionControlAuthDispatcher;
import site.aster.interceptors.CapabilityInterceptor;
import site.aster.interceptors.Interceptor;
import site.aster.server.AsterServer;
import site.aster.server.spi.ServiceDispatcher;

/**
 * Auth-mode Mission Control server. Wires the role-gated {@link MissionControlAuthDispatcher} and
 * {@link AgentSessionAuthDispatcher} in place of the dev-mode dispatchers and attaches a {@link
 * CapabilityInterceptor} so {@code @Requires}-style gates fire before handlers run.
 *
 * <p>Java port of {@code python -m examples.mission_control.server --auth}. Like the Python
 * equivalent, this is Chapter-5 territory — it proves the gating path end-to-end without yet
 * depending on the full admission credential pipeline (Phase 3). Roles are provided via a {@link
 * MetadataRoleInterceptor} that copies {@code metadata["aster.role"]} into {@code
 * ctx.attributes()}; swap this out for a credential-driven attribute setter when Phase 3 lands.
 */
public final class ServerAuth {

  public static void main(String[] args) throws Exception {
    ForyCodec codec = new ForyCodec();
    Server.registerWireTypes(codec);

    MissionControl missionControl = new MissionControl();

    boolean strict = false;
    for (String a : args) {
      if ("--strict".equals(a)) strict = true;
    }

    AsterServer server =
        (strict
                ? buildStrictAuth(codec, missionControl, AgentSession::new)
                : buildWithAuth(codec, missionControl, AgentSession::new))
            .get(15, TimeUnit.SECONDS);

    System.out.println("Mission Control auth server started");
    System.out.println("  node id   : " + server.nodeId());
    site.aster.node.NodeAddr addr = server.node().nodeAddr();
    System.out.println("  node addr : " + addr);
    System.out.println(addr.toTicket());
    System.out.println("  services  :");
    server.manifest().forEach(d -> System.out.println("    - " + d.name() + " v" + d.version()));
    System.out.println("Ctrl-C to stop.");

    Runtime.getRuntime()
        .addShutdownHook(
            new Thread(
                () -> {
                  System.out.println("Stopping Mission Control auth server…");
                  server.close();
                }));

    Thread.currentThread().join();
  }

  /**
   * Compose an {@link AsterServer} wired with auth-mode dispatchers and a capability-gated
   * interceptor chain. Shared by {@link #main} and the E2E tests so both exercise identical wiring.
   * Returns the pre-build {@link java.util.concurrent.CompletableFuture} so callers can chain their
   * own bootstrap work on top.
   */
  public static java.util.concurrent.CompletableFuture<AsterServer> buildWithAuth(
      ForyCodec codec,
      MissionControl missionControl,
      java.util.function.Function<String, AgentSession> agentFactory) {
    MissionControlAuthDispatcher mcDispatcher = new MissionControlAuthDispatcher();
    AgentSessionAuthDispatcher asDispatcher = new AgentSessionAuthDispatcher();

    // Build the dispatcher map up-front so CapabilityInterceptor can be wired into the builder
    // chain BEFORE the server starts. This mirrors Python's auto-wiring inside AsterServer, where
    // the gate is assembled from the registered services at server construction.
    Map<String, ServiceDispatcher> services =
        Map.of(
            MissionControlAuthDispatcher.SERVICE_NAME, mcDispatcher,
            AgentSessionAuthDispatcher.SERVICE_NAME, asDispatcher);

    List<Interceptor> chain =
        List.of(new MetadataRoleInterceptor(), new CapabilityInterceptor(services));

    return AsterServer.builder()
        .codec(codec)
        .interceptors(chain)
        .service(missionControl, mcDispatcher)
        .sessionService(AgentSession.class, agentFactory::apply, asDispatcher)
        .build();
  }

  /**
   * Strict variant of {@link #buildWithAuth} that requires every admitting peer to present a
   * non-empty credential; empty credentials are denied at the admission handshake itself. Mirrors
   * the Python {@code ASTER_ALLOW_ALL_CONSUMERS=false} setting used by the matrix auth tests.
   */
  public static java.util.concurrent.CompletableFuture<AsterServer> buildStrictAuth(
      ForyCodec codec,
      MissionControl missionControl,
      java.util.function.Function<String, AgentSession> agentFactory) {
    MissionControlAuthDispatcher mcDispatcher = new MissionControlAuthDispatcher();
    AgentSessionAuthDispatcher asDispatcher = new AgentSessionAuthDispatcher();

    Map<String, ServiceDispatcher> services =
        Map.of(
            MissionControlAuthDispatcher.SERVICE_NAME, mcDispatcher,
            AgentSessionAuthDispatcher.SERVICE_NAME, asDispatcher);

    List<Interceptor> chain =
        List.of(new MetadataRoleInterceptor(), new CapabilityInterceptor(services));

    AsterConfig config = AsterConfig.builder().allowAllConsumers(false).build();

    return AsterServer.builder()
        .codec(codec)
        .config(config)
        .interceptors(chain)
        .service(missionControl, mcDispatcher)
        .sessionService(AgentSession.class, agentFactory::apply, asDispatcher)
        .build();
  }

  private ServerAuth() {}
}
