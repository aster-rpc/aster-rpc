package site.aster.examples.missioncontrol;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.file.Files;
import java.nio.file.Path;
import java.security.SecureRandom;
import java.time.Duration;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;
import site.aster.client.AsterClient;
import site.aster.client.ClientSession;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.enrollment.EnrollmentConfig;
import site.aster.examples.missioncontrol.types.Command;
import site.aster.examples.missioncontrol.types.CommandResult;
import site.aster.examples.missioncontrol.types.IngestResult;
import site.aster.examples.missioncontrol.types.LogEntry;
import site.aster.examples.missioncontrol.types.MetricPoint;
import site.aster.examples.missioncontrol.types.StatusRequest;
import site.aster.examples.missioncontrol.types.StatusResponse;
import site.aster.examples.missioncontrol.types.SubmitLogResult;
import site.aster.examples.missioncontrol.types.TailRequest;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * The auth chapter end to end (the Java version of the Rust {@code tests/auth.rs} and Go's {@code
 * mc/auth_test.go}): the server is configured from an {@code aster.toml} exactly as an operator
 * writes it; credentials are the files {@code aster enroll node --attributes} produces (signed here
 * by {@link TestRoot}); clients are {@link AsterClient}s configured with their own key and
 * credential. Each role reaches exactly the methods whose {@code requires} it satisfies.
 */
@Timeout(value = 180)
final class MissionControlAuthE2ETest {

  static final String AUDIENCE = "mission-control";
  static final SecureRandom RANDOM = new SecureRandom();

  @TempDir Path dir;

  private final List<AutoCloseable> closers = new ArrayList<>();
  private TestRoot root;
  private String fleet;
  private NodeAddr server;
  private MissionControl missionControl;

  @AfterEach
  void closeAll() {
    for (int i = closers.size() - 1; i >= 0; i--) {
      try {
        closers.get(i).close();
      } catch (Exception ignored) {
        // swallow-ok: best-effort teardown; the resource may already be closed
      }
    }
  }

  /** {@code aster key node generate --out <name>.key}: 64 hex. Returns the seed. */
  byte[] newKey(String name) throws Exception {
    byte[] seed = new byte[32];
    RANDOM.nextBytes(seed);
    Files.writeString(dir.resolve(name + ".key"), HexFormat.of().formatHex(seed) + "\n");
    return seed;
  }

  /** What {@code aster enroll node --subject <id> --attributes roles.json --out} writes. */
  void enroll(byte[] seed, String roles, String name) throws Exception {
    String chain =
        root.credential(TestRoot.nodeId(seed), TestRoot.roleClaims(roles), Duration.ofHours(1));
    Files.writeString(dir.resolve(name + ".cred"), chain + "\n");
  }

  void startServer() throws Exception {
    // `aster key root generate`, `aster enroll namespace create`.
    root = TestRoot.generate(AUDIENCE);
    byte[] secret = new byte[32];
    RANDOM.nextBytes(secret);
    fleet = TestRoot.namespace(secret);
    enroll(newKey("server"), "", "server");
    Path toml = dir.resolve("aster.toml");
    Files.writeString(
        toml,
        """
        [trust]
        root_pubkey = "%s"
        enrollment_credential = "%s"

        [network]
        secret_key_file = "%s"
        relay_mode = "disabled"
        bind_addr = "127.0.0.1:0"

        [enrollment]
        audience = "%s"
        fleet_namespace = "%s"
        state_path = "%s"
        """
            .formatted(
                root.publicKeyHex(),
                dir.resolve("server.cred"),
                dir.resolve("server.key"),
                AUDIENCE,
                fleet,
                dir.resolve("server-enrollment.json")));
    AsterConfig config = AsterConfig.fromFile(toml);
    assertTrue(config.isEnrollmentEnabled());
    ForyCodec codec = new ForyCodec();
    Server.registerWireTypes(codec);
    missionControl = new MissionControl();
    AsterServer srv =
        ServerAuth.build(config, codec, missionControl, AgentSession::new)
            .get(60, TimeUnit.SECONDS);
    closers.add(srv);
    server = NodeAddr.fromTicket(srv.node().nodeAddr().toTicket());
  }

  /** A client with its own key and a credential carrying {@code roles}. */
  AsterClient connect(String name, String roles) throws Exception {
    enroll(newKey(name), roles, name);
    AsterConfig config =
        AsterConfig.builder()
            .relayMode("disabled")
            .bindAddr("127.0.0.1:0")
            .secretKeyFile(dir.resolve(name + ".key").toString())
            .rootPubkey(root.publicKey())
            .enrollmentCredentialFile(dir.resolve(name + ".cred").toString())
            .enrollment(EnrollmentConfig.builder().audience(AUDIENCE).fleetNamespace(fleet).build())
            .build();
    return client(config);
  }

  AsterClient client(AsterConfig config) throws Exception {
    ForyCodec codec = new ForyCodec();
    Server.registerWireTypes(codec);
    AsterClient c =
        AsterClient.builder().config(config).codec(codec).build().get(60, TimeUnit.SECONDS);
    closers.add(c);
    return c;
  }

  static <T> T get(CompletableFuture<T> f) throws Exception {
    return f.get(30, TimeUnit.SECONDS);
  }

  static boolean denied(ThrowingCall call) {
    try {
      call.run();
      return false;
    } catch (ExecutionException e) {
      return e.getCause() instanceof RpcError r && r.code() == StatusCode.PERMISSION_DENIED;
    } catch (Exception e) {
      return e instanceof RpcError r && r.code() == StatusCode.PERMISSION_DENIED;
    }
  }

  @FunctionalInterface
  interface ThrowingCall {
    void run() throws Exception;
  }

  StatusResponse status(AsterClient c) throws Exception {
    return get(
        c.call(
            server, "MissionControl", "getStatus", new StatusRequest("a1"), StatusResponse.class));
  }

  /** {@code tailLogs} is open-ended: one queued entry proves the call passed Gate 3. */
  LogEntry tail(AsterClient c) throws Exception {
    LogEntry marker = new LogEntry(1.0d, "info", "authorized", "a1");
    missionControl.logQueue().offer(marker);
    try (var stream =
        get(
            c.openServerStream(
                server,
                "MissionControl",
                "tailLogs",
                new TailRequest("", "debug"),
                LogEntry.class))) {
      return stream.recvWithTimeout(15, TimeUnit.SECONDS);
    }
  }

  IngestResult ingest(AsterClient c) throws Exception {
    return get(
        c.callClientStream(
            server,
            "MissionControl",
            "ingestMetrics",
            List.of(new MetricPoint("cpu", 1, 0, Map.of())),
            IngestResult.class));
  }

  List<CommandResult> command(AsterClient c) throws Exception {
    try (ClientSession session = get(c.openSession(server))) {
      return get(
          session.<Command, CommandResult>callBidiStream(
              "AgentSession", "runCommand", List.of(new Command("uptime")), CommandResult.class));
    }
  }

  @Test
  void rolesInCredentialsGateEachMethod() throws Exception {
    startServer();

    // An operator watches the fleet.
    AsterClient operator = connect("operator", Role.STATUS + "," + Role.LOGS);
    assertEquals("running", status(operator).status());
    assertEquals("authorized", tail(operator).message());
    assertTrue(denied(() -> ingest(operator)), "operator ingestMetrics");
    assertTrue(denied(() -> command(operator)), "operator runCommand");

    // An agent reports in.
    AsterClient agent = connect("agent", Role.STATUS + "," + Role.INGEST);
    assertEquals(1, ingest(agent).accepted());
    assertTrue(denied(() -> tail(agent)), "agent tailLogs");

    // An administrator runs commands but has no status role.
    AsterClient admin = connect("admin", Role.ADMIN);
    assertEquals("ran: uptime", command(admin).get(0).stdout());
    assertEquals("authorized", tail(admin).message());
    assertTrue(denied(() -> status(admin)), "admin getStatus");

    // Every enrolled node may submit logs; no role needed.
    AsterClient plain = connect("plain", "");
    assertTrue(
        get(plain.call(
                server,
                "MissionControl",
                "submitLog",
                new LogEntry(1.0d, "info", "hello", "a1"),
                SubmitLogResult.class))
            .accepted());
    assertTrue(denied(() -> status(plain)), "plain getStatus");

    // A node with no credential cannot connect at all.
    AsterClient stranger =
        client(AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build());
    try {
      get(
          stranger.call(
              server,
              "MissionControl",
              "submitLog",
              new LogEntry(1.0d, "info", "hello", "a1"),
              SubmitLogResult.class));
      throw new AssertionError("a node without a credential reached the service");
    } catch (ExecutionException expected) {
      // refused at Gate 0
    }
  }
}
