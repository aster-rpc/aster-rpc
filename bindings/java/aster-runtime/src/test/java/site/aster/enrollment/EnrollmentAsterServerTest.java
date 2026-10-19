package site.aster.enrollment;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;
import site.aster.annotations.Scope;
import site.aster.client.AsterClient;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.contract.CapabilityKind;
import site.aster.contract.CapabilityRequirement;
import site.aster.docs.AuthorId;
import site.aster.docs.Doc;
import site.aster.docs.Docs;
import site.aster.interceptors.CallContext;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * Enrollment through the high-level path only, mirroring {@code
 * aster/tests/enrollment_aster_server.rs} (and Go's {@code enrollment_test.go}): {@link
 * AsterServer} and {@link AsterClient} configured by {@link AsterConfig}'s trust fields and {@code
 * [enrollment]} section. The root is played by {@link TestRoot} (root operations belong to the
 * CLI); everything the application writes is configuration and {@code requires}.
 *
 * <p>These are wiring tests. Runtime role restriction and issuer renewal are behaviour, tested once
 * in Rust ({@code enrollment_aster_server.rs} {@code runtime_restriction_narrows_credential_roles},
 * {@code config_driven_issuer_renews_client_credentials}).
 */
@Timeout(value = 180)
final class EnrollmentAsterServerTest {

  static final String FLEET = "mission-control";
  static final String MSG = "_aster_test/EnrollOps";
  static final TestRoot ROOT = new TestRoot(seed(1), FLEET);
  static final byte[] FLEET_SECRET = filled(0x55);
  static final String FLEET_NS = TestRoot.namespace(FLEET_SECRET);

  @TempDir Path dir;

  private final List<AutoCloseable> closers = new ArrayList<>();

  /** The request and reply: the reply carries the caller's {@code aster.role}. */
  public record Ops(String roles, int n) {}

  public static final class Impl {}

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

  static byte[] filled(int b) {
    byte[] out = new byte[32];
    Arrays.fill(out, (byte) b);
    return out;
  }

  static byte[] seed(int b) {
    return filled(b);
  }

  static String pub(int b) {
    return TestRoot.nodeId(seed(b));
  }

  static String credentialFor(int seed, String roles) {
    return ROOT.credential(pub(seed), TestRoot.roleClaims(roles), Duration.ofHours(1));
  }

  /** A loopback node with the root pinned, the credential inline and the fleet's section. */
  static AsterConfig.Builder enrolled(int seed, String credential) {
    return AsterConfig.builder()
        .relayMode("disabled")
        .bindAddr("127.0.0.1:0")
        .secretKey(seed(seed))
        .rootPubkey(ROOT.publicKey())
        .enrollmentCredentialChain(credential.isEmpty() ? null : credential)
        .enrollment(EnrollmentConfig.builder().audience(FLEET).fleetNamespace(FLEET_NS).build());
  }

  /** The server (seed 2): a root-issued credential, durable state and fast reconciliation. */
  AsterConfig.Builder serverCfg() {
    return enrolled(2, credentialFor(2, ""))
        .enrollment(
            EnrollmentConfig.builder()
                .audience(FLEET)
                .fleetNamespace(FLEET_NS)
                .statePath(dir.resolve("server-enrollment.json").toString())
                .limits(EnrollmentLimits.DEFAULTS.withReconciliationIntervalMs(200L))
                .build());
  }

  static MethodDescriptor desc(String name, CapabilityRequirement requires) {
    return new MethodDescriptor(
        name,
        StreamingKind.UNARY,
        RequestStyle.EXPLICIT,
        MSG,
        List.of(),
        MSG,
        false,
        false,
        requires);
  }

  /** Ops: status requires ops.status, logs ops.logs or ops.admin, ping is open. */
  static final class OpsService implements ServiceDispatcher {
    @Override
    public ServiceDescriptor descriptor() {
      return new ServiceDescriptor("Ops", 1, Scope.SHARED, Impl.class);
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      return Map.of(
          "ping", echo(desc("ping", null)),
          "status",
              echo(
                  desc(
                      "status",
                      new CapabilityRequirement(CapabilityKind.ROLE, List.of("ops.status")))),
          "logs",
              echo(
                  desc(
                      "logs",
                      new CapabilityRequirement(
                          CapabilityKind.ANY_OF, List.of("ops.logs", "ops.admin")))));
    }

    @Override
    public void registerTypes(org.apache.fory.Fory fory) {
      try {
        fory.register(Ops.class, MSG);
      } catch (Throwable ignored) {
        // swallow-ok: already registered by an earlier test in this process
      }
    }

    private static UnaryDispatcher echo(MethodDescriptor d) {
      return new UnaryDispatcher() {
        @Override
        public MethodDescriptor descriptor() {
          return d;
        }

        @Override
        public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
          return c.encode(new Ops(ctx.attributes().getOrDefault("aster.role", ""), 0));
        }
      };
    }
  }

  static ForyCodec codec() {
    ForyCodec codec = new ForyCodec();
    try {
      codec.fory().register(Ops.class, MSG);
    } catch (Throwable ignored) {
      // swallow-ok: already registered by an earlier test in this process
    }
    return codec;
  }

  AsterServer startOps(AsterServer.Builder b) throws Exception {
    AsterServer server =
        b.codec(codec()).service(new Impl(), new OpsService()).build().get(60, TimeUnit.SECONDS);
    closers.add(server);
    return server;
  }

  AsterClient client(AsterConfig config) throws Exception {
    AsterClient c =
        AsterClient.builder().config(config).codec(codec()).build().get(60, TimeUnit.SECONDS);
    closers.add(c);
    return c;
  }

  static String call(AsterClient c, NodeAddr server, String method) throws Exception {
    return c.call(server, "Ops", method, new Ops("", 1), Ops.class)
        .get(20, TimeUnit.SECONDS)
        .roles();
  }

  static Throwable failure(CompletableFuture<?> f) throws Exception {
    try {
      f.get(30, TimeUnit.SECONDS);
    } catch (ExecutionException e) {
      return e.getCause();
    }
    throw new AssertionError("expected a failure");
  }

  static Throwable callFailure(AsterClient c, NodeAddr server, String method) throws Exception {
    return failure(c.call(server, "Ops", method, new Ops("", 1), Ops.class));
  }

  static boolean denied(Throwable t) {
    return t instanceof RpcError e && e.code() == StatusCode.PERMISSION_DENIED;
  }

  /**
   * The roles in a client's credential are what {@code requires} checks, with no application code;
   * a credential without the role is refused, and a node with no credential cannot reach the
   * service. A revocation the root publishes refuses the next call and closes the connection.
   */
  @Test
  void credentialRolesGateRequiresAndRootRevocationClosesTheConnection() throws Exception {
    // No replicas: the server holds the fleet namespace and the root publishes into it, as
    // `aster enroll revoke --peer <server>` does.
    AsterServer server = startOps(AsterServer.builder().config(serverCfg().build()));
    NodeAddr addr = server.node().nodeAddr();
    Enrollment enrollment = server.enrollment();
    assertNotNull(enrollment);
    assertNull(server.issuer(), "no issuer configured");
    EnrollmentStatus st = enrollment.status();
    assertEquals("server", st.side());
    assertEquals(ROOT.publicKeyHex(), st.trustedRoot());
    assertEquals(FLEET_NS, st.fleetNamespace());
    assertEquals(pub(2), st.credential().subject());

    AsterClient operator = client(enrolled(5, credentialFor(5, "ops.status, ops.logs")).build());
    for (String m : List.of("ping", "status", "logs")) {
      assertEquals("ops.logs,ops.status", call(operator, addr, m), m);
    }
    assertFalse(operator.isRenewing(), "a root-issued credential has no issuer to renew from");

    AsterClient ingest = client(enrolled(6, credentialFor(6, "ops.ingest")).build());
    assertEquals("ops.ingest", call(ingest, addr, "ping"));
    for (String m : List.of("status", "logs")) {
      assertTrue(denied(callFailure(ingest, addr, m)), m);
    }
    // The server sees the verified claims (audit), and maps them itself.
    PeerEvidence ev = enrollment.peerClaims(pub(6)).orElseThrow();
    assertEquals("ops.ingest", ev.attributes().get("aster.role").value());

    // No enrollment at all: Gate 0 refuses the connection itself.
    AsterClient stranger =
        client(AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build());
    callFailure(stranger, addr, "ping");

    // The root enrolls with the server (a publication session) and delivers a revocation of the
    // operator into the fleet namespace the server holds.
    CompletableFuture<Revocation> revoked = enrollment.nextRevocation();
    AsterClient root = client(enrolled(1, credentialFor(1, "")).build());
    root.connect(addr).get(30, TimeUnit.SECONDS);
    Docs docs = new Docs(root.endpoint().runtime(), root.endpoint().nativeHandle());
    try (Doc fleet = docs.importWriteNamespaceAsync(FLEET_SECRET).get(30, TimeUnit.SECONDS)) {
      fleet
          .setBytesAsync(
              AuthorId.of(ROOT.publicKeyHex()),
              TestRoot.registrationKey(pub(5)),
              ROOT.revocation(pub(5), "decommissioned"))
          .get(30, TimeUnit.SECONDS);
      fleet.startSyncAsync(List.of(pub(2))).get(30, TimeUnit.SECONDS);
      Revocation rev = revoked.get(30, TimeUnit.SECONDS);
      assertEquals(pub(5), rev.peer());
      assertEquals("node_revoked", rev.reason().code());
      assertTrue(rev.reason().definite());
    }
    assertNotNull(callFailure(operator, addr, "status"), "revoked: the next call is refused");
    assertNotNull(callFailure(operator, addr, "ping"), "revoked: the connection is closed");
    assertTrue(enrollment.peerClaims(pub(5)).isEmpty());
    Throwable check = failure(enrollment.checkPeer(pub(5)));
    AdmissionException admission = assertInstanceOf(AdmissionException.class, check);
    assertNotEquals(AdmissionException.Kind.OTHER, admission.kind());
    // Unaffected peers keep working.
    assertEquals("ops.ingest", call(ingest, addr, "ping"));
  }

  /** Builder overrides of the policy and role mapping take precedence over the configuration. */
  @Test
  void roleMappingOverride() throws Exception {
    AsterServer server =
        startOps(
            AsterServer.builder()
                .config(serverCfg().build())
                // Scopes are roles; the aster.role attribute grants nothing.
                .enrollmentRoles(
                    new RoleMapping(
                        "", true, Map.of(), Map.of("oncall", List.of("ops.logs")), null)));
    Map<String, Object> claims = new java.util.LinkedHashMap<>(TestRoot.roleClaims("ops.admin"));
    claims.put("scopes", List.of("ops.status"));
    claims.put("tags", List.of("oncall"));
    String chain = ROOT.credential(pub(5), claims, Duration.ofHours(1));
    AsterClient c = client(enrolled(5, chain).build());
    assertEquals("ops.logs,ops.status", call(c, server.node().nodeAddr(), "status"));
  }

  void refused(String name, AsterServer.Builder b, String expect) throws Exception {
    Throwable t = failure(b.codec(codec()).service(new Impl(), new OpsService()).build());
    String msg = String.valueOf(t.getMessage());
    assertTrue(msg.contains(expect), name + ": \"" + expect + "\" not in " + msg);
  }

  /** Misconfiguration stops the start with an error that names the problem; nothing runs open. */
  @Test
  void misconfigurationFailsClosed() throws Exception {
    EnrollmentConfig good = serverCfg().build().enrollment();
    refused(
        "no audience",
        AsterServer.builder()
            .config(serverCfg().enrollment(good.toBuilder().audience(null).build()).build()),
        "audience is required");
    refused(
        "no root",
        AsterServer.builder().config(serverCfg().rootPubkey(null).build()),
        "no trusted root");
    refused(
        "no state",
        AsterServer.builder()
            .config(serverCfg().enrollment(good.toBuilder().statePath(null).build()).build()),
        "keeps denials durably");
    // A server with no credential, or one that does not verify for its key,
    // starts: only a root registration admits it (aster/tests/
    // enrollment_aster_server.rs registered_server_restarts_without_a_valid_credential).
    refused(
        "a path where the chain belongs",
        AsterServer.builder()
            .config(serverCfg().enrollmentCredentialChain("/etc/aster/server.cred").build()),
        "ASTER_ENROLLMENT_CREDENTIAL)");
    refused(
        "unknown policy",
        AsterServer.builder()
            .config(
                serverCfg()
                    .enrollment(
                        good.toBuilder().policy(Policy.fromJson("\"allow_everyone\"")).build())
                    .build()),
        "policy");
    refused(
        "policy override",
        AsterServer.builder().config(serverCfg().build()).enrollmentPolicy(Policy.allOf()),
        "policy");
    refused(
        "namespace",
        AsterServer.builder()
            .config(
                serverCfg().enrollment(good.toBuilder().fleetNamespace("not-hex").build()).build()),
        "fleet_namespace");
    refused(
        "enabled alone",
        AsterServer.builder()
            .config(
                enrolled(2, credentialFor(2, ""))
                    .enrollment(EnrollmentConfig.builder().enabled(true).build())
                    .build()),
        "audience is required");

    // A client: no credential, and server-only issuer settings.
    Throwable t =
        failure(AsterClient.builder().config(enrolled(5, "").build()).codec(codec()).build());
    assertTrue(String.valueOf(t.getMessage()).contains("no credential"), t.getMessage());
    AsterConfig withIssuer =
        enrolled(5, credentialFor(5, ""))
            .enrollment(
                EnrollmentConfig.builder()
                    .audience(FLEET)
                    .fleetNamespace(FLEET_NS)
                    .issuerDelegationFile(dir.resolve("x").toString())
                    .build())
            .build();
    t = failure(AsterClient.builder().config(withIssuer).codec(codec()).build());
    assertTrue(String.valueOf(t.getMessage()).contains("server-only"), t.getMessage());

    // Enrollment off (the root alone does not enable it): an open server.
    AsterConfig open =
        AsterConfig.builder()
            .relayMode("disabled")
            .bindAddr("127.0.0.1:0")
            .rootPubkey(ROOT.publicKey())
            .build();
    assertFalse(open.isEnrollmentEnabled(), "the root alone enabled enrollment");
    AsterServer server = startOps(AsterServer.builder().config(open));
    assertNull(server.enrollment(), "enrollment installed without being configured");
  }

  @Test
  void policyJson() {
    Policy p =
        Policy.allOf(
            Policy.anyOf(Policy.scope("session"), Policy.tag("router")),
            Policy.noneOf(Policy.issuedBy("0".repeat(64))),
            Policy.attributeEquals("tier", Attribute.integer(2)),
            Policy.sourceCidr("10.0.0.0/8"),
            Policy.audience("kasm"),
            Policy.synchronizedWithin(Duration.ofMinutes(1), true),
            Policy.maxCredentialAge(Duration.ofHours(1)),
            Policy.requireRegistration(),
            Policy.tolerateStale());
    String want =
        "{\"all_of\":[{\"any_of\":[{\"scope\":\"session\"},{\"tag\":\"router\"}]},{\"none_of\":[{\"issued_by\":\""
            + "0".repeat(64)
            + "\"}]},{\"attribute_equals\":{\"key\":\"tier\",\"value\":{\"type\":\"int\",\"value\":2}}},"
            + "{\"source_cidr\":\"10.0.0.0/8\"},{\"audience\":\"kasm\"},{\"policy_synchronized_within\":"
            + "{\"max_age_secs\":60,\"require_root\":true}},{\"max_credential_age_secs\":3600},"
            + "\"require_registration\",\"tolerate_stale_policy\"]}";
    assertEquals(want, p.toJson());
    assertEquals(p, Policy.fromJson(want), "a policy read from configuration round-trips");
  }

  @Test
  void enablementRule() {
    assertFalse(AsterConfig.builder().build().isEnrollmentEnabled());
    assertFalse(AsterConfig.builder().rootPubkey(ROOT.publicKey()).build().isEnrollmentEnabled());
    assertTrue(
        AsterConfig.builder().enrollmentCredentialFile("x.cred").build().isEnrollmentEnabled());
    assertTrue(
        AsterConfig.builder()
            .enrollment(EnrollmentConfig.builder().audience("a").build())
            .build()
            .isEnrollmentEnabled());
    assertFalse(
        AsterConfig.builder()
            .enrollmentCredentialFile("x.cred")
            .enrollment(EnrollmentConfig.builder().enabled(false).build())
            .build()
            .isEnrollmentEnabled());
  }
}
