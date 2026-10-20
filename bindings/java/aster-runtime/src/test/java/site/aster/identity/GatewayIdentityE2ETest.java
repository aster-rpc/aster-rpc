package site.aster.identity;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.node.JsonNodeFactory;
import java.math.BigInteger;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
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
import site.aster.audit.AuditOutbox;
import site.aster.client.AsterClient;
import site.aster.client.CallOptions;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.contract.CapabilityKind;
import site.aster.contract.CapabilityRequirement;
import site.aster.enrollment.EnrollmentConfig;
import site.aster.enrollment.EnrollmentLimits;
import site.aster.enrollment.TestRoot;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;
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
 * Gateway identity (C ABI 7.7) on the high-level path only: {@link AsterServer} and {@link
 * AsterClient} configured by {@link AsterConfig}'s {@code [identity]} / {@code [remote_config]}
 * fields and the credentials, mirroring Go's {@code identity_test.go} (and {@code
 * aster/tests/enrollment_gateway_identity.rs}, {@code enrollment_provider_tokens.rs}, {@code
 * node_config.rs}). The handler reads everything from {@link CallContext#current()}.
 *
 * <p>These are wiring tests. Refusing the entry when accept_on_behalf_of is off and verifying
 * provider tokens against published keys are behaviour, tested once in Rust ({@code
 * enrollment_gateway_identity.rs} {@code
 * a_server_that_does_not_accept_on_behalf_of_refuses_the_entry}, {@code
 * enrollment_provider_tokens.rs} {@code
 * provider_tokens_verify_with_published_keys_rotate_and_follow_node_config}).
 */
@Timeout(value = 180)
final class GatewayIdentityE2ETest {

  static final String FLEET = "mission-control";
  static final String MSG = "test.gateway/Who";
  static final TestRoot ROOT = new TestRoot(filled(1), FLEET);
  static final byte[] FLEET_SECRET = filled(0x55);
  static final String FLEET_NS = TestRoot.namespace(FLEET_SECRET);

  @TempDir Path dir;

  private final List<AutoCloseable> closers = new ArrayList<>();

  /** Request and reply (a plain class: the wire type). */
  public static final class Who {
    public String principal = "";
    public String peer = "";
    public String actor = "";
    public String amr = "";
    public String roles = "";
    public String issuer = "";
    public long tier;
    public String name = "";
    public String claimsSub = "none";

    public Who() {}
  }

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

  static String pub(int seed) {
    return TestRoot.nodeId(filled(seed));
  }

  /** Claims with {@code roles} in {@code aster.role} and, when given, an issuer delegation. */
  static Map<String, Object> claimsWith(String roles, String issuable) {
    Map<String, Object> claims = new LinkedHashMap<>(TestRoot.roleClaims(roles));
    @SuppressWarnings("unchecked")
    Map<String, Object> attributes =
        new LinkedHashMap<>((Map<String, Object>) claims.get("attributes"));
    if (!issuable.isEmpty()) {
      attributes.put(
          GatewayIdentity.ISSUABLE_ROLES_ATTRIBUTE, Map.of("type", "string", "value", issuable));
    }
    claims.put("attributes", attributes);
    return claims;
  }

  static String credential(int seed, Map<String, Object> claims) {
    return ROOT.credential(pub(seed), claims, Duration.ofHours(1));
  }

  static AsterConfig.Builder enrolled(int seed, String chain) {
    return AsterConfig.builder()
        .relayMode("disabled")
        .bindAddr("127.0.0.1:0")
        .secretKey(filled(seed))
        .rootPubkey(ROOT.publicKey())
        .enrollmentCredentialChain(chain)
        .enrollment(EnrollmentConfig.builder().audience(FLEET).fleetNamespace(FLEET_NS).build());
  }

  /** The server (seed 2): durable state, fast reconciliation, on-behalf-of accepted. */
  AsterConfig.Builder serverCfg() {
    return enrolled(2, credential(2, TestRoot.roleClaims("")))
        .enrollment(
            EnrollmentConfig.builder()
                .audience(FLEET)
                .fleetNamespace(FLEET_NS)
                .statePath(dir.resolve("server-enrollment.json").toString())
                .limits(EnrollmentLimits.DEFAULTS.withReconciliationIntervalMs(200L))
                .build())
        .acceptOnBehalfOf(true);
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

  /** What the handler sees, read from {@code CallContext.current()} (a ScopedValue). */
  static Who who() {
    CallContext ctx = CallContext.current();
    Who w = new Who();
    w.principal = ctx.principal();
    w.peer = ctx.peer();
    w.actor = ctx.actor() == null ? "" : ctx.actor();
    w.amr = String.join(",", ctx.authMethods());
    w.roles = ctx.attributes().getOrDefault("aster.role", "");
    w.issuer = ctx.onBehalfOf() == null ? "" : ctx.onBehalfOf().issuer();
    NodeAttribute tier = ctx.nodeAttribute("tier");
    w.tier = tier == null ? 0 : tier.asInt().orElse(0L);
    w.name = ctx.nodeProperties().name();
    w.claimsSub = ctx.tokenClaims() == null ? "none" : String.valueOf(ctx.tokenClaims().get("sub"));
    return w;
  }

  /** Notes: whoami is open, read needs notes.read, publish notes.read and a passkey sign-in. */
  static final class NotesService implements ServiceDispatcher {
    @Override
    public ServiceDescriptor descriptor() {
      return new ServiceDescriptor("Notes", 1, Scope.SHARED, Impl.class);
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      CapabilityRequirement read =
          new CapabilityRequirement(CapabilityKind.ROLE, List.of("notes.read"));
      return Map.of(
          "whoami", unary(desc("whoami", null), false),
          "read", unary(desc("read", read), false),
          "publish", unary(desc("publish", read), true));
    }

    @Override
    public void registerTypes(org.apache.fory.Fory fory) {
      register(fory);
    }

    private static UnaryDispatcher unary(MethodDescriptor d, boolean passkey) {
      return new UnaryDispatcher() {
        @Override
        public MethodDescriptor descriptor() {
          return d;
        }

        @Override
        public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
          if (passkey) {
            CallContext.current().requireAuthMethod(GatewayIdentity.AMR_WEBAUTHN);
          }
          return c.encode(who());
        }
      };
    }
  }

  static void register(org.apache.fory.BaseFory fory) {
    try {
      fory.register(Who.class, MSG);
    } catch (Throwable ignored) {
      // swallow-ok: already registered by an earlier test in this process
    }
  }

  static ForyCodec codec() {
    ForyCodec codec = new ForyCodec();
    register(codec.fory());
    return codec;
  }

  AsterServer start(AsterServer.Builder b) throws Exception {
    AsterServer server =
        b.codec(codec()).service(new Impl(), new NotesService()).build().get(60, TimeUnit.SECONDS);
    closers.add(server);
    return server;
  }

  AsterClient client(AsterConfig config) throws Exception {
    AsterClient c =
        AsterClient.builder().config(config).codec(codec()).build().get(60, TimeUnit.SECONDS);
    closers.add(c);
    return c;
  }

  static Who call(AsterClient c, NodeAddr server, String method, CallOptions options)
      throws Exception {
    return c.call(
            server,
            "Notes",
            method,
            new Who(),
            Who.class,
            options.withTimeout(Duration.ofSeconds(20)))
        .get(30, TimeUnit.SECONDS);
  }

  static Who call(AsterClient c, NodeAddr server, String method) throws Exception {
    return call(c, server, method, CallOptions.DEFAULT);
  }

  static <T> T get(CompletableFuture<T> f) throws Exception {
    return f.get(30, TimeUnit.SECONDS);
  }

  static Throwable failure(CompletableFuture<?> f) throws Exception {
    try {
      f.get(30, TimeUnit.SECONDS);
    } catch (ExecutionException e) {
      return e.getCause();
    }
    throw new AssertionError("expected a failure");
  }

  static void wantCode(Throwable t, StatusCode code, String what) {
    RpcError e = assertInstanceOf(RpcError.class, t, what + ": " + t);
    assertEquals(code, e.code(), what + ": " + e.getMessage());
  }

  static Throwable callFailure(AsterClient c, NodeAddr server, String method, CallOptions o) {
    try {
      call(c, server, method, o);
    } catch (ExecutionException e) {
      return e.getCause();
    } catch (Exception e) {
      return e;
    }
    throw new AssertionError(method + ": expected a failure");
  }

  /**
   * An edge holding aster.gateway mints a token for a user; the handler sees the user, the edge as
   * the actor and the passkey sign-in. The edge, holding aster.config-admin, changes the server
   * through NodeConfig (audited): the handler sees the attributes, and turning accept_on_behalf_of
   * off refuses the entry.
   */
  @Test
  void edgeMintedTokenAndNodeConfig() throws Exception {
    AsterServer server =
        start(
            AsterServer.builder()
                .config(serverCfg().build())
                .auditOutbox(
                    new AuditOutbox.Config(
                        dir.resolve("audit"),
                        new AuditOutbox.Destination("http://127.0.0.1:9/audit"))));
    NodeAddr addr = server.node().nodeAddr();
    assertNotNull(server.auditOutbox());
    AsterClient edge =
        client(
            enrolled(
                    5,
                    credential(
                        5,
                        claimsWith(
                            GatewayIdentity.GATEWAY_ROLE + "," + GatewayIdentity.CONFIG_ADMIN_ROLE,
                            "notes.read")))
                .build());
    String edgeId = pub(5);

    Who me = call(edge, addr, "whoami");
    assertEquals(edgeId, me.principal);
    assertEquals("none", me.claimsSub); // no verified forwarded token, no claims
    assertEquals("", me.actor);
    assertEquals("", me.amr);
    wantCode(
        callFailure(edge, addr, "read", CallOptions.DEFAULT),
        StatusCode.PERMISSION_DENIED,
        "the edge itself lacks notes.read");

    String token =
        edge.mintUserToken(
            "alice",
            List.of("notes.read"),
            List.of(GatewayIdentity.AMR_WEBAUTHN),
            Duration.ofMinutes(1));
    Who alice = call(edge, addr, "publish", CallOptions.DEFAULT.withOnBehalfOf(token));
    assertEquals(edgeId + "|alice", alice.principal);
    assertEquals(edgeId, alice.peer);
    assertEquals(edgeId, alice.actor);
    assertEquals(GatewayIdentity.AMR_WEBAUTHN, alice.amr);
    assertEquals("notes.read", alice.roles);
    assertEquals(edgeId, alice.issuer);
    assertEquals("alice", alice.claimsSub); // the verified token's claims

    String pwd =
        edge.mintUserToken(
            "bob",
            List.of("notes.read"),
            List.of(GatewayIdentity.AMR_PASSWORD),
            Duration.ofMinutes(1));
    wantCode(
        callFailure(edge, addr, "publish", CallOptions.DEFAULT.withOnBehalfOf(pwd)),
        StatusCode.PERMISSION_DENIED,
        "publish needs a passkey");
    IrohException outside =
        assertThrows(
            IrohException.class,
            () ->
                edge.mintUserToken(
                    "bob",
                    List.of("notes.admin"),
                    List.of(GatewayIdentity.AMR_PASSWORD),
                    Duration.ofMinutes(1)));
    assertEquals(IrohStatus.INVALID_ARGUMENT, outside.status(), outside.getMessage());
    IrohException noPublisher =
        assertThrows(
            IrohException.class, () -> edge.enrollment().noticeKid("https://idp.test", "k1"));
    assertEquals(IrohStatus.NOT_FOUND, noPublisher.status());

    // NodeConfig: get, set, ack, clear from the admin edge.
    NodeConfigClient admin = get(edge.nodeConfig(addr)).withDeadline(Duration.ofSeconds(20));
    NodeConfigView view = get(admin.get());
    assertEquals(pub(2), view.nodeId());
    NodeConfigView.EffectiveSetting accept = view.setting("identity.accept_on_behalf_of");
    assertNotNull(accept, view.toString());
    assertTrue(accept.value().asBoolean(), accept.toString());
    assertEquals("code", accept.source());

    NodeConfigAck ack =
        get(
            admin.set(
                NodeConfigSetRequest.empty()
                    .withName("notes-1")
                    .withTags(List.of("eu"))
                    .withAttribute("tier", NodeAttribute.ofInt(3))
                    .withIdentity(IdentityOverride.NONE.withAcceptOnBehalfOf(false))));
    assertTrue(ack.complete(), ack.toString());
    assertEquals(1, ack.generation());
    me = call(edge, addr, "whoami");
    assertEquals(3, me.tier);
    assertEquals("notes-1", me.name);
    NodeProperties props = server.nodeProperties();
    assertEquals(1, props.generation());
    assertTrue(props.hasTag("eu"));
    wantCode(
        callFailure(edge, addr, "read", CallOptions.DEFAULT.withOnBehalfOf(token)),
        StatusCode.PERMISSION_DENIED,
        "accept_on_behalf_of = false refuses the entry");
    assertTrue(get(admin.ack(1, Duration.ofSeconds(1))).complete());
    wantCode(
        failure(
            admin.set(
                NodeConfigSetRequest.empty()
                    .withSection(
                        "network", JsonNodeFactory.instance.objectNode().put("bind_port", 1)))),
        StatusCode.PERMISSION_DENIED,
        "a section that is not overridable");
    NodeConfigAck cleared = get(admin.clear(0));
    assertTrue(cleared.complete(), cleared.toString());
    alice = call(edge, addr, "read", CallOptions.DEFAULT.withOnBehalfOf(token));
    assertEquals(edgeId, alice.actor);
    assertEquals(0, alice.tier);
    // The set and the clear went to the durable audit outbox.
    AuditOutbox.Status st = get(server.auditOutbox().status());
    assertTrue(st.committedSequence().compareTo(BigInteger.ONE) >= 0, st.toString());

    // A peer without aster.config-admin is refused by Gate 3, and cannot forward.
    AsterClient plain =
        client(enrolled(6, credential(6, TestRoot.roleClaims("notes.read"))).build());
    wantCode(
        failure(get(plain.nodeConfig(addr)).get()),
        StatusCode.PERMISSION_DENIED,
        "NodeConfig needs aster.config-admin");
    wantCode(
        callFailure(plain, addr, "read", CallOptions.DEFAULT.withOnBehalfOf(token)),
        StatusCode.PERMISSION_DENIED,
        "forwarding needs aster.gateway");
  }

  /** An audit outbox needs NodeConfig: with [remote_config] enabled = false the start fails. */
  @Test
  void auditOutboxWithNodeConfigOffFailsTheStart() throws Exception {
    CompletableFuture<AsterServer> f =
        AsterServer.builder()
            .config(serverCfg().remoteConfigEnabled(false).build())
            .auditOutbox(
                new AuditOutbox.Config(
                    dir.resolve("audit-off"), new AuditOutbox.Destination("http://127.0.0.1:9/a")))
            .codec(codec())
            .service(new Impl(), new NotesService())
            .build();
    Throwable t = failure(f);
    assertTrue(String.valueOf(t.getMessage()).contains("UNSUPPORTED"), String.valueOf(t));
  }

  @Test
  void bearerToken() {
    assertEquals("abc.def", GatewayIdentity.bearerToken("Bearer abc.def").orElseThrow());
    assertEquals("x", GatewayIdentity.bearerToken("  bearer   x ").orElseThrow());
    assertTrue(GatewayIdentity.bearerToken("Basic x").isEmpty());
    assertTrue(GatewayIdentity.bearerToken(null).isEmpty());
  }

  /** The per-call token replaces any earlier entry, whatever its case. */
  @Test
  void onBehalfOfReplacesTheEntry() {
    CallOptions o =
        CallOptions.DEFAULT
            .withMetadata("Aster-On-Behalf-Of", "old")
            .withMetadata("k", "v")
            .withOnBehalfOf("new");
    assertEquals(Map.of("k", "v", GatewayIdentity.ON_BEHALF_OF_METADATA, "new"), o.metadata());
    assertEquals("new", o.onBehalfOf());
    assertNull(CallOptions.DEFAULT.onBehalfOf());
  }

  /** requireAuthMethod refuses a call made as the peer itself, and a weaker sign-in. */
  @Test
  void requireAuthMethodOnTheContext() {
    CallContext self = CallContext.builder("s", "m").peer("p").build();
    assertNull(self.actor());
    assertTrue(self.authMethods().isEmpty());
    RpcError e =
        assertThrows(RpcError.class, () -> self.requireAuthMethod(GatewayIdentity.AMR_WEBAUTHN));
    assertEquals(StatusCode.PERMISSION_DENIED, e.code());
    CallContext fwd =
        CallContext.builder("s", "m")
            .peer("gw")
            .principal("iss|alice")
            .actor("gw")
            .onBehalfOf(
                new ForwardedIdentity(
                    "iss", "alice", "", null, List.of(GatewayIdentity.AMR_PASSWORD)))
            .build();
    fwd.requireAuthMethod(GatewayIdentity.AMR_PASSWORD);
    assertThrows(RpcError.class, () -> fwd.requireAuthMethod(GatewayIdentity.AMR_WEBAUTHN));
    assertEquals(NodeProperties.EMPTY, fwd.nodeProperties());
    assertNull(fwd.nodeAttribute("tier"));
  }
}
