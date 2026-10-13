package site.aster.examples.missioncontrol;

import java.nio.file.Path;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.function.Function;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.examples.missioncontrol.auth.AgentSessionAuthDispatcher;
import site.aster.examples.missioncontrol.auth.MissionControlAuthDispatcher;
import site.aster.server.AsterServer;

/**
 * Mission Control, the auth chapter: the role-gated {@link MissionControlAuthDispatcher} and {@link
 * AgentSessionAuthDispatcher} served by an {@link AsterServer} whose Enrollment comes entirely from
 * configuration ({@code ASTER_*} variables, or an {@code aster.toml} with {@code --config}).
 *
 * <p>Nothing here reads a credential. With enrollment configured the server admits only nodes
 * holding a credential from your root (produced with the Rust {@code aster} CLI; see the README),
 * and each peer's {@code aster.role} claim becomes the role set every method's {@code requires} is
 * checked against, in Rust, on every call. A root revocation refuses the peer's next call and
 * closes its connection. The same role names as the Python, TypeScript, Rust and Go examples
 * ({@link Role}).
 *
 * <pre>
 *   ASTER_ROOT_PUBKEY=… ASTER_ENROLLMENT_AUDIENCE=mission-control \
 *   ASTER_ENROLLMENT_FLEET_NAMESPACE=… ASTER_SECRET_KEY_FILE=server.key \
 *   ASTER_ENROLLMENT_CREDENTIAL=server.cred ASTER_ENROLLMENT_STATE_PATH=./mc-enrollment.json \
 *   mvn -pl aster-examples-mission-control exec:java \
 *     -Dexec.mainClass=site.aster.examples.missioncontrol.ServerAuth
 * </pre>
 */
public final class ServerAuth {

  private ServerAuth() {}

  public static void main(String[] args) throws Exception {
    Path configFile = null;
    for (int i = 0; i < args.length; i++) {
      if ("--config".equals(args[i]) && i + 1 < args.length) {
        configFile = Path.of(args[++i]);
      }
    }
    AsterConfig config =
        configFile == null ? AsterConfig.fromEnv() : AsterConfig.fromFile(configFile);
    if (!config.isEnrollmentEnabled()) {
      System.err.println(
          "the auth chapter needs enrollment configured: ASTER_ROOT_PUBKEY,"
              + " ASTER_ENROLLMENT_AUDIENCE, ASTER_ENROLLMENT_FLEET_NAMESPACE,"
              + " ASTER_ENROLLMENT_CREDENTIAL and ASTER_ENROLLMENT_STATE_PATH (see the README)");
      System.exit(2);
    }
    ForyCodec codec = new ForyCodec();
    Server.registerWireTypes(codec);
    AsterServer server =
        build(config, codec, new MissionControl(), AgentSession::new).get(60, TimeUnit.SECONDS);

    System.out.println("Mission Control auth server started");
    System.out.println("  node id   : " + server.nodeId());
    site.aster.node.NodeAddr addr = server.node().nodeAddr();
    System.out.println("  node addr : " + addr);
    // The aster1… ticket on its own line, last among the address lines (the matrix harness
    // greps for it).
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
   * The auth-chapter server on {@code config}: the role-gated services and nothing else. Shared by
   * {@link #main} and the tests so both run identical wiring.
   */
  public static CompletableFuture<AsterServer> build(
      AsterConfig config,
      ForyCodec codec,
      MissionControl missionControl,
      Function<String, AgentSession> agentFactory) {
    return AsterServer.builder()
        .config(config)
        .codec(codec)
        .service(missionControl, new MissionControlAuthDispatcher())
        .sessionService(AgentSession.class, agentFactory::apply, new AgentSessionAuthDispatcher())
        .build();
  }
}
