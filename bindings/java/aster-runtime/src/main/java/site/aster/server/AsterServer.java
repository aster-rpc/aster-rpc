package site.aster.server;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.ServiceLoader;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Function;
import site.aster.annotations.Scope;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.contract.ContractManifestBuilder;
import site.aster.ffi.AsterEnvelope;
import site.aster.ffi.Dispatch;
import site.aster.ffi.IrohException;
import site.aster.interceptors.CallContext;
import site.aster.interceptors.Interceptor;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.IrohNode;
import site.aster.otel.Observability;
import site.aster.otel.TraceContext;
import site.aster.otel.Tracing;
import site.aster.server.spi.BidiStreamDispatcher;
import site.aster.server.spi.ClientStreamDispatcher;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.ServerStreamDispatcher;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.UnaryDispatcher;
import site.aster.server.wire.RpcStatus;

/**
 * High-level Aster RPC server on dynamic dispatch (C ABI 3.5, {@code aster_server_*}).
 *
 * <p>Discovers {@link ServiceDispatcher}s via {@link ServiceLoader} and binds each to a user
 * instance (SHARED) or a per-session factory (SESSION). Each service is lowered to its JSON
 * registration form ({@link ContractManifestBuilder#dynamicService}) and served by Rust's {@code
 * aster::rpc::Server}: Rust decodes headers, routes, enforces {@code @Requires} (Gate 3), keeps
 * sessions per connection with the §7.5 graveyard and cap, enforces deadlines and per-method frame
 * limits, and serves {@code aster.ops.NodeInfo}. This class drains the dispatch events on one
 * platform thread, runs each call on a virtual thread (client-stream / bidi on a platform thread,
 * since their request pulls block in native code), runs the interceptor chain, and answers by call
 * id with payloads and {@code RpcStatus}.
 *
 * <p>Manifest submission runs at {@link Builder#build()} (non-fatal); hook dispatch is not wired.
 *
 * <p><b>Enrollment</b> (design §15 of {@code docs/_internal/working_ideas/aster-enrollment.md}):
 * with enrollment configured in the {@link AsterConfig} ({@code [trust]} and {@code [enrollment]},
 * {@code ASTER_ENROLLMENT_*}; {@link AsterConfig#isEnrollmentEnabled()}), the server admits only
 * nodes holding a credential from the configured root, and each peer's verified claims become the
 * roles {@code @Requires} checks, on every call, with no application code. A root revocation
 * refuses the peer's next call and closes its connections. A configured issuer ({@code
 * [enrollment.issuer]}) serves credential renewals. Any missing or malformed setting fails {@link
 * Builder#build()} naming the setting; nothing falls back to an open node. Without enrollment
 * configured the server is open. Serve enrollment-gated services over Iroh only: HTTP callers never
 * get enrollment roles.
 */
public final class AsterServer implements AutoCloseable {

  public static final String ASTER_ALPN = "aster/1";
  private static final int DEFAULT_RING_CAPACITY = 256;
  private static final int DEFAULT_POLL_BATCH = 64;
  private static final int POLL_TIMEOUT_MS = 100;

  /**
   * Default per-connection session cap (spec §7.5 / §9 — {@code
   * aster.transport.max_sessions_per_connection}). Enforced in Rust; configurable via {@link
   * Builder#maxSessionsPerConnection}.
   */
  static final int DEFAULT_MAX_SESSIONS_PER_CONNECTION = 1024;

  private final IrohNode node;
  private final Dispatch dispatch;
  private final Codec codec;
  private final Map<String, RegisteredService> services;

  /** {@code service_idx} → service (registration order). */
  private final RegisteredService[] byIndex;

  private final List<Interceptor> interceptors;
  private final AsterConfig config;
  private final List<ServiceDescriptor> manifest;
  private final AtomicBoolean running = new AtomicBoolean(true);
  private final AtomicBoolean draining = new AtomicBoolean(false);
  private final Thread pollThread;
  private final ExecutorService callExecutor;
  private final ExecutorService streamingExecutor;

  /** The configured delegated issuer (null = none). */
  private final site.aster.enrollment.EnrollmentIssuer issuer;

  /** The serializer behind {@link Builder#leaseGrantor} (null = none). */
  private final site.aster.lease.LeaseGrantor.Backend grantor;

  /** The ops listener attached to this server (null = none); closed with the server if owned. */
  private final site.aster.health.HealthServer health;

  private final boolean ownsHealth;

  /** The NodeConfig audit outbox opened for {@link Builder#auditOutbox} (null = none). */
  private final site.aster.audit.AuditOutbox auditOutbox;

  /** {@link #nodeProperties()} cached per NodeConfig generation. */
  private volatile site.aster.identity.NodeProperties cachedProperties;

  /** The {@code aster.ops.NodeInfo} attribute announcing the contract registry namespace. */
  public static final String REGISTRY_NAMESPACE_ATTRIBUTE = "aster.registry_namespace";

  /** 64-char hex doc-id of the registry doc created by the contract publisher; empty until then. */
  private volatile String registryNamespace = "";

  /** Populated by {@link site.aster.contract.ContractPublisher} after publication. */
  private final Map<String, String> publishedContractIds = new ConcurrentHashMap<>();

  /**
   * Session instances by Rust's {@code session_key}. Created on the poll thread on a session's
   * first call and dropped on {@code SESSION_CLOSED}, so a call can never resurrect a reaped
   * session (the ring delivers a session's calls before its close).
   */
  private final Map<Long, Object> sessions = new ConcurrentHashMap<>();

  /** Test-only per-connection view (see {@link #debugConnectionSnapshot()}); poll thread writes. */
  private final Map<Long, ConnectionView> connections = new ConcurrentHashMap<>();

  private final Map<Long, Long> sessionConnection = new HashMap<>();

  /**
   * Per-peer admission attributes. Mirrored into Rust's attribute store (Gate 3 reads it) and
   * merged into every {@link CallContext#attributes()} for calls from that peer.
   */
  private final Map<String, Map<String, String>> peerAttributes = new ConcurrentHashMap<>();

  /** Live calls whose handler asked {@link CallContext#cancelled()}: completed on CANCELLED. */
  private final Map<Long, CompletableFuture<Void>> cancelSignals = new ConcurrentHashMap<>();

  private AsterServer(
      Builder b,
      IrohNode node,
      Dispatch dispatch,
      RegisteredService[] byIndex,
      site.aster.health.HealthServer health,
      boolean ownsHealth,
      site.aster.lease.LeaseGrantor.Backend grantor,
      site.aster.audit.AuditOutbox auditOutbox) {
    this.node = node;
    this.auditOutbox = auditOutbox;
    this.grantor = grantor;
    this.health = health;
    this.ownsHealth = ownsHealth;
    this.dispatch = dispatch;
    this.codec = b.codec != null ? b.codec : new ForyCodec();
    this.services = Map.copyOf(b.services);
    this.byIndex = byIndex;
    this.interceptors = List.copyOf(b.interceptors);
    this.config = b.config;
    this.manifest = buildManifest(this.services.values());
    this.callExecutor = Executors.newVirtualThreadPerTaskExecutor();
    // Client-stream / bidi handlers pull requests through aster_dispatch_recv, a blocking native
    // call that would pin a virtual thread's carrier: they run on platform threads.
    this.streamingExecutor =
        Executors.newCachedThreadPool(
            r -> {
              Thread t = new Thread(r, "aster-server-streaming");
              t.setDaemon(true);
              return t;
            });
    site.aster.enrollment.Enrollment enrollment = node.enrollment();
    this.issuer =
        enrollment != null && enrollment.status().issuer() != null
            ? new site.aster.enrollment.EnrollmentIssuer(enrollment)
            : null;
    this.pollThread =
        Thread.ofPlatform().daemon(true).name("aster-server-poll").start(this::pollLoop);
  }

  /** 64-char hex doc-id of the registry doc hosting this server's manifests; empty until set. */
  public String registryNamespace() {
    return registryNamespace;
  }

  void setPublicationResult(String namespace, Map<String, String> contractIds) {
    this.registryNamespace = namespace == null ? "" : namespace;
    this.publishedContractIds.putAll(contractIds);
    if (!this.registryNamespace.isEmpty()) {
      // Clients find the contract registry through aster.ops.NodeInfo (the
      // retired consumer-admission response used to carry it).
      try {
        dispatch.setNodeAttribute(REGISTRY_NAMESPACE_ATTRIBUTE, this.registryNamespace);
      } catch (RuntimeException e) {
        System.err.println("AsterServer: could not announce the contract registry: " + e);
      }
    }
  }

  /** The node ID of this server (hex string). */
  public String nodeId() {
    return node.nodeId();
  }

  /** The ops listener attached via {@link Builder#health} or started from config, or null. */
  public site.aster.health.HealthServer health() {
    return health;
  }

  public IrohNode node() {
    return node;
  }

  public AsterConfig config() {
    return config;
  }

  /**
   * The Enrollment installed from the config (null when enrollment is off). Roles and revocations
   * need nothing from it; it is for audit ({@code peerClaims}), checks before a sensitive operation
   * ({@code checkPeer}) and revocation events ({@code nextRevocation}).
   */
  public site.aster.enrollment.Enrollment enrollment() {
    return node.enrollment();
  }

  /**
   * The delegated issuer the server runs when {@code [enrollment.issuer] delegation_file} is
   * configured (null otherwise). It already serves renewals; use {@code issue} to enroll new nodes.
   */
  public site.aster.enrollment.EnrollmentIssuer issuer() {
    return issuer;
  }

  /**
   * The serving node's {@code aster.ops.NodeConfig} properties now (Rust {@code
   * AsterServer::node_config().properties()}; {@code aster_server_node_properties}), cached per
   * generation. {@link site.aster.identity.NodeProperties#EMPTY} once closed.
   */
  public site.aster.identity.NodeProperties nodeProperties() {
    if (!running.get()) {
      return site.aster.identity.NodeProperties.EMPTY;
    }
    long generation = dispatch.nodeConfigGeneration();
    site.aster.identity.NodeProperties cached = cachedProperties;
    if (cached != null && cached.generation() == generation) {
      return cached;
    }
    byte[] json = dispatch.nodePropertiesJson();
    site.aster.identity.NodeProperties fresh =
        json.length == 0
            ? site.aster.identity.NodeProperties.EMPTY
            : site.aster.identity.IdentityJson.decode(
                json, site.aster.identity.NodeProperties.class);
    cachedProperties = fresh;
    return fresh;
  }

  /** The NodeConfig audit outbox opened for {@link Builder#auditOutbox} (null = none). */
  public site.aster.audit.AuditOutbox auditOutbox() {
    return auditOutbox;
  }

  /**
   * The serialization point behind {@link Builder#leaseGrantor} (null without one): the grantor's
   * own rows, for snapshots and for co-located holders on this node.
   */
  public site.aster.lease.NativeLeaseSerializer leaseSerializer() {
    return grantor == null ? null : grantor.serializer();
  }

  /**
   * <strong>TEST-ONLY</strong>. Per-connection session view, {@code connectionId → (active
   * sessions, highest opened session id)}, rebuilt from the dispatch events (the authoritative
   * state, graveyard and cap, lives in Rust). Entries are dropped on connection close.
   */
  public Map<Long, ConnectionSnapshot> debugConnectionSnapshot() {
    Map<Long, ConnectionSnapshot> out = new HashMap<>();
    for (Map.Entry<Long, ConnectionView> e : connections.entrySet()) {
      ConnectionView v = e.getValue();
      synchronized (v) {
        out.put(e.getKey(), new ConnectionSnapshot(v.active.size(), v.lastOpened));
      }
    }
    return out;
  }

  /** Test-only snapshot record; see {@link #debugConnectionSnapshot()}. */
  public record ConnectionSnapshot(int activeSessionCount, int lastOpenedSessionId) {}

  private static final class ConnectionView {
    final Set<Long> active = new HashSet<>();
    int lastOpened;
  }

  public List<ServiceDescriptor> manifest() {
    return manifest;
  }

  /** Live view of the registered {@link ServiceDispatcher}s keyed by service name. */
  public Map<String, ServiceDispatcher> serviceDispatchers() {
    LinkedHashMap<String, ServiceDispatcher> out = new LinkedHashMap<>();
    for (Map.Entry<String, RegisteredService> e : services.entrySet()) {
      out.put(e.getKey(), e.getValue().dispatcher());
    }
    return java.util.Collections.unmodifiableMap(out);
  }

  /**
   * Attach admission attributes (e.g. {@code aster.role}) to every subsequent call from {@code
   * peerId}: Rust's Gate 3 checks {@code @Requires} against them, and they are merged into {@link
   * CallContext#attributes()}. Pass {@code null} or an empty map to clear.
   *
   * <p>With enrollment configured, the credential sets the maximum and runtime settings can only
   * restrict: a peer with a live enrollment session takes its roles from its credential ({@code
   * [enrollment.roles]}), so {@code aster.role} here fails with an {@code INVALID_ARGUMENT} {@link
   * site.aster.ffi.IrohException} (narrow it with {@link #restrictPeerRoles}); other keys apply,
   * and the credential's attributes win on a clash. For a peer without a session (no enrollment, or
   * a custom gate) these attributes, {@code aster.role} included, apply as given.
   */
  public void setPeerAttributes(String peerId, Map<String, String> attributes) {
    if (peerId == null) {
      return;
    }
    if (attributes == null || attributes.isEmpty()) {
      peerAttributes.remove(peerId);
      dispatch.setPeerAttributes(peerId, Map.of());
      return;
    }
    dispatch.setPeerAttributes(peerId, attributes);
    peerAttributes.put(peerId, Map.copyOf(attributes));
  }

  /**
   * Narrow an enrolled peer's roles to {@code roles} intersected with its credential roles, from
   * its next call (calls in flight finish). The credential sets the maximum: a role it lacks is
   * never granted, and an empty list refuses every role-gated call. Replaces an earlier
   * restriction, survives renewals of the same credential subject, and ends with the peer's
   * enrollment session (revocation, eviction, expiry).
   *
   * @throws site.aster.ffi.IrohException {@code NOT_FOUND} when {@code peerId} has no live
   *     enrollment session
   */
  public void restrictPeerRoles(String peerId, java.util.Collection<String> roles) {
    dispatch.restrictPeerRoles(peerId, String.join(",", roles));
  }

  /** Remove {@code peerId}'s role restriction; its credential roles apply from its next call. */
  public void clearPeerRestriction(String peerId) {
    dispatch.clearPeerRestriction(peerId);
  }

  /** Drop any admission attributes for {@code peerId}. */
  public void clearPeerAttributes(String peerId) {
    setPeerAttributes(peerId, null);
  }

  /** The canonical {@link site.aster.contract.ContractManifest} JSON for one service. */
  public String manifestJson(String serviceName) {
    RegisteredService rs = services.get(serviceName);
    if (rs == null) {
      return null;
    }
    return ContractManifestBuilder.build(rs.dispatcher()).toJson();
  }

  /** The {@code contract_id} of a registered service; {@code null} if unknown. */
  public String contractId(String serviceName) {
    RegisteredService rs = services.get(serviceName);
    return rs == null ? null : rs.contractId;
  }

  /** Live calls in Rust's call table (diagnostics / tests). */
  public long liveCalls() {
    return dispatch.liveCalls();
  }

  /**
   * Start a bind-ready authenticated HTTPS/optional WebTransport listener over these services.
   * Register {@code upcalls.serveRpcAuthenticator(...)} first. The table must use {@code
   * node().runtime()}; this server's close also stops and joins every child listener.
   * Authentication supplies the principal and Gate 3 attributes and does not grant Iroh transport
   * provenance.
   */
  public HttpListener startHttp(site.aster.upcall.Upcalls upcalls, HttpListenerConfig config) {
    if (!running.get()) {
      throw IrohException.forStatus(site.aster.ffi.IrohStatus.ALREADY_CLOSED, "server closed");
    }
    return dispatch.startHttp(upcalls, config);
  }

  /** Explicit anonymous HTTPS/WebTransport; requires no authentication table. */
  public HttpListener startPublicHttp(HttpListenerConfig config) {
    if (!running.get())
      throw IrohException.forStatus(site.aster.ffi.IrohStatus.ALREADY_CLOSED, "server closed");
    return dispatch.startPublicHttp(config);
  }

  /**
   * Take the server out of rotation, then close it (ABI 7.2, {@code aster::AsterServer::drain}):
   *
   * <ol>
   *   <li>an attached health listener reports not ready;
   *   <li>new connections are closed right after their handshake with {@link
   *       DrainReport#DRAIN_CLOSE_CODE}, and new calls answer {@code UNAVAILABLE} with the {@code
   *       aster.draining} detail ({@link RpcError#isDraining()}); calls on a session that already
   *       exists still run;
   *   <li>each connection is closed once it has no call in flight and no live session;
   *   <li>calls still running at {@code grace} are cancelled by closing their connection (handlers
   *       see {@link CallContext#isCancelled()}), and the drain waits up to 2 s for them to end.
   * </ol>
   *
   * <p>Blocks until done. A concurrent {@code drain} with a shorter grace cuts a running one short
   * ({@link Duration#ZERO} ends it now) and returns the same report. The server is closed whatever
   * happens.
   *
   * @throws IllegalArgumentException if {@code grace} is negative
   * @throws IrohException {@code ALREADY_CLOSED} if the server is closed
   */
  public DrainReport drain(Duration grace) {
    Objects.requireNonNull(grace, "grace");
    if (grace.isNegative()) {
      throw new IllegalArgumentException("drain grace must be nonnegative");
    }
    if (!running.get()) {
      throw IrohException.forStatus(site.aster.ffi.IrohStatus.ALREADY_CLOSED, "server closed");
    }
    draining.set(true);
    long ms =
        grace.compareTo(Duration.ofMillis(0xFFFF_FFFFL)) >= 0
            ? 0xFFFF_FFFFL
            : grace.toMillis() + (grace.toNanosPart() % 1_000_000 != 0 ? 1 : 0);
    try {
      return dispatch.drain(ms);
    } finally {
      close();
    }
  }

  /** Whether {@link #drain} has started on this server. */
  public boolean isDraining() {
    return draining.get();
  }

  @Override
  public void close() {
    if (!running.compareAndSet(true, false)) {
      return;
    }
    try {
      pollThread.join(2000);
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
    }
    callExecutor.shutdown();
    streamingExecutor.shutdown();
    try {
      callExecutor.awaitTermination(2, TimeUnit.SECONDS);
      streamingExecutor.awaitTermination(2, TimeUnit.SECONDS);
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
    }
    for (Object instance : sessions.values()) {
      closeIfAutoCloseable(instance);
    }
    sessions.clear();
    connections.clear();
    dispatch.close(); // aster_server_destroy clears the attached listener's ready flag first
    if (grantor != null) {
      grantor.close();
    }
    if (ownsHealth && health != null) {
      health.close();
    }
    if (auditOutbox != null) {
      try {
        auditOutbox.closeAsync().get(5, TimeUnit.SECONDS);
      } catch (Exception ignored) {
        // best effort: the runtime closes with the node
      }
    }
    node.close();
  }

  // ───── Poll loop ─────────────────────────────────────────────────────────

  /** One call, copied out of its event on the poll thread. */
  private record Inbound(
      long callId,
      RegisteredService service,
      int methodIdx,
      int flags,
      Object instance,
      long deadlineUnixMs,
      int sessionId,
      byte[] request,
      String peer,
      String transportPeer,
      String actor,
      int wireCallId,
      long connectionHandle,
      java.util.concurrent.atomic.AtomicBoolean over) {}

  private void pollLoop() {
    long size = Dispatch.EVENT_LAYOUT.byteSize();
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment events = arena.allocate(Dispatch.EVENT_LAYOUT, DEFAULT_POLL_BATCH);
      while (running.get()) {
        int count = dispatch.poll(events, DEFAULT_POLL_BATCH, POLL_TIMEOUT_MS);
        for (int i = 0; i < count; i++) {
          MemorySegment ev = events.asSlice(i * size, size);
          try {
            onEvent(ev);
          } catch (RuntimeException e) {
            System.err.println("aster-server-poll: event failed: " + e);
          }
        }
      }
    }
  }

  private void onEvent(MemorySegment ev) {
    byte kind = ev.get(ValueLayout.JAVA_BYTE, Dispatch.OFFSET_KIND);
    switch (kind) {
      case Dispatch.EVENT_CALL -> onCall(ev);
      case Dispatch.EVENT_RESPONSE_WRITABLE, Dispatch.EVENT_RESPONSE_CLOSED ->
          dispatch.handleCapacityEvent(
              kind, ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_CALL_ID));
      case Dispatch.EVENT_CANCELLED -> {
        // Rust already answered (deadline / corrupt request) or there is nobody to answer (peer
        // cancel); handlers see it through isCancelled(), cancelled() and GONE results.
        CompletableFuture<Void> signal =
            cancelSignals.remove(ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_CALL_ID));
        if (signal != null) {
          signal.complete(null);
        }
      }
      case Dispatch.EVENT_SESSION_CLOSED -> {
        long key = ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_SESSION_KEY);
        closeIfAutoCloseable(sessions.remove(key));
        Long conn = sessionConnection.remove(key);
        ConnectionView view = conn == null ? null : connections.get(conn);
        if (view != null) {
          synchronized (view) {
            view.active.remove(key);
          }
        }
      }
      case Dispatch.EVENT_CONNECTION_CLOSED -> {
        connections.remove(ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_CONNECTION_ID));
        dispatch.bufferRelease(ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_BUFFER));
      }
      case Dispatch.EVENT_NON_RPC -> onNonRpc(ev);
      default -> System.err.println("aster-server-poll: unknown dispatch event kind " + kind);
    }
  }

  private void onCall(MemorySegment ev) {
    long callId = ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_CALL_ID);
    int serviceIdx = ev.get(ValueLayout.JAVA_INT, Dispatch.OFFSET_SERVICE_IDX);
    int methodIdx = ev.get(ValueLayout.JAVA_INT, Dispatch.OFFSET_METHOD_IDX);
    int flags = ev.get(ValueLayout.JAVA_INT, Dispatch.OFFSET_FLAGS);
    long sessionKey = ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_SESSION_KEY);
    int sessionId = ev.get(ValueLayout.JAVA_INT, Dispatch.OFFSET_SESSION_ID);
    long connectionId = ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_CONNECTION_ID);
    long connectionHandle = ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_CONNECTION_HANDLE);
    long deadline = ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_DEADLINE_UNIX_MS);
    int wireCallId = ev.get(ValueLayout.JAVA_INT, Dispatch.OFFSET_WIRE_CALL_ID);
    byte[] request = bytes(ev, Dispatch.OFFSET_REQUEST_PTR, Dispatch.OFFSET_REQUEST_LEN);
    // peer_ptr is the principal (ABI 7.5); with HAS_PRINCIPAL the transport peer differs and is
    // read now, while the call is live.
    String peer =
        new String(
            bytes(ev, Dispatch.OFFSET_PEER_PTR, Dispatch.OFFSET_PEER_LEN), StandardCharsets.UTF_8);
    dispatch.bufferRelease(ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_BUFFER));
    String transportPeer =
        resolveTransportPeer(
            flags,
            peer,
            (flags & Dispatch.FLAG_HAS_PRINCIPAL) != 0 ? dispatch.identity(callId) : null);
    if (transportPeer == null) {
      // NOT_FOUND: the call left Rust's table (cancelled or past its deadline) before this read,
      // and Rust already answered it. Nothing runs for it.
      return;
    }
    // A forwarded call (ABI 7.7): the gateway is the transport peer, the actor.
    String actor = (flags & Dispatch.FLAG_ON_BEHALF_OF) != 0 ? transportPeer : null;

    RegisteredService svc = serviceIdx < byIndex.length ? byIndex[serviceIdx] : null;
    if (svc == null || methodIdx < 0 || methodIdx >= svc.methods.length) {
      fail(callId, StatusCode.UNIMPLEMENTED, "no handler for service " + serviceIdx, null);
      return;
    }
    Object instance = svc.sharedInstance;
    if (sessionKey != 0) {
      // Rust allocated the key and decided the session may exist (graveyard, cap): here it only
      // needs an instance. Created on this thread, ahead of the session's SESSION_CLOSED.
      instance = sessions.computeIfAbsent(sessionKey, k -> svc.factory.apply(peer));
      if ((flags & Dispatch.FLAG_NEW_SESSION) != 0) {
        trackSession(connectionId, sessionKey, sessionId);
      }
    }
    Inbound call =
        new Inbound(
            callId,
            svc,
            methodIdx,
            flags,
            instance,
            deadline,
            sessionId,
            request,
            peer,
            transportPeer,
            actor,
            wireCallId,
            connectionHandle,
            new AtomicBoolean());
    callExecutor.execute(() -> dispatchCall(call));
  }

  /**
   * A call's transport peer: {@code peer} (the event's) without {@code HAS_PRINCIPAL}, else the one
   * {@code aster_dispatch_identity} read. {@code null} when that read found the call over ({@code
   * NOT_FOUND}): the call is dropped rather than reporting the principal (the user, on a forwarded
   * call) as the transport peer and the actor.
   */
  static String resolveTransportPeer(int flags, String peer, Dispatch.Identity identity) {
    if ((flags & Dispatch.FLAG_HAS_PRINCIPAL) == 0) {
      return peer;
    }
    if (identity == null) {
      return null;
    }
    return identity.peer().isEmpty() ? peer : identity.peer();
  }

  private void trackSession(long connectionId, long sessionKey, int sessionId) {
    ConnectionView view = connections.computeIfAbsent(connectionId, id -> new ConnectionView());
    synchronized (view) {
      view.active.add(sessionKey);
      view.lastOpened = Math.max(view.lastOpened, sessionId);
    }
    sessionConnection.put(sessionKey, connectionId);
  }

  private void onNonRpc(MemorySegment ev) {
    dispatch.bufferRelease(ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_BUFFER));
    long handle = ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_CONNECTION_HANDLE);
    // An extra ALPN of the config that Rust does not serve itself: owned by us, closed.
    new site.aster.handle.IrohConnection(node.runtime(), handle).close();
  }

  private static byte[] bytes(MemorySegment ev, long ptrOffset, long lenOffset) {
    int len = ev.get(ValueLayout.JAVA_INT, lenOffset);
    if (len == 0) {
      return new byte[0];
    }
    return ev.get(ValueLayout.ADDRESS, ptrOffset).reinterpret(len).toArray(ValueLayout.JAVA_BYTE);
  }

  // ───── Dispatch ──────────────────────────────────────────────────────────

  private void dispatchCall(Inbound call) {
    long callId = call.callId();
    long probeT2 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;
    try {
      MethodDispatcher method = call.service().methods[call.methodIdx()];
      CallContext ctx = buildCallContext(call, method);
      long probeT3 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;

      // The call's server span and inbound baggage are the current trace context (a ScopedValue
      // plus the SLF4J MDC) for everything the handler runs, on whichever thread that is.
      TraceContext trace = Tracing.fromDispatch(dispatch, callId, call.flags(), ctx.metadata());

      // Server interceptors run pre-dispatch with the raw request (Gate 3 already ran in Rust).
      if (!interceptors.isEmpty()) {
        Tracing.runIn(
            trace,
            () ->
                site.aster.interceptors.InterceptorChain.applyRequest(
                    interceptors, ctx, call.request()));
      }
      if (Thread.currentThread().isVirtual()
          && (method instanceof ClientStreamDispatcher || method instanceof BidiStreamDispatcher)) {
        streamingExecutor.execute(
            () -> Tracing.runIn(trace, () -> runDispatch(call, method, ctx, probeT2, probeT3)));
        return;
      }
      Tracing.runIn(trace, () -> runDispatch(call, method, ctx, probeT2, probeT3));
    } catch (Throwable t) {
      call.over().set(true);
      cancelSignals.remove(callId);
      failWith(callId, t);
    }
  }

  /** The signal behind {@link CallContext#cancelled()} for a live call. */
  private CompletableFuture<Void> cancelSignal(long callId) {
    CompletableFuture<Void> signal =
        cancelSignals.computeIfAbsent(callId, id -> new CompletableFuture<>());
    // The CANCELLED event may have been handled before the signal existed.
    if (!signal.isDone() && dispatch.isCancelled(callId)) {
      cancelSignals.remove(callId);
      signal.complete(null);
    }
    return signal;
  }

  private void runDispatch(
      Inbound call, MethodDispatcher method, CallContext ctx, long probeT2, long probeT3) {
    try {
      CallContext.runScoped(ctx, () -> runHandler(call, method, ctx, probeT2, probeT3));
    } finally {
      call.over().set(true);
      cancelSignals.remove(call.callId());
    }
  }

  private void runHandler(
      Inbound call, MethodDispatcher method, CallContext ctx, long probeT2, long probeT3) {
    long callId = call.callId();
    Object instance = call.instance();
    try {
      switch (method) {
        case UnaryDispatcher u -> {
          long probeT4 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;
          byte[] response = u.invoke(instance, call.request(), codec, ctx);
          long probeT5 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;
          dispatch.respond(callId, response, null);
          if (site.aster.probe.AsterProbes.ENABLED) {
            site.aster.probe.AsterProbes.recordServer(
                probeT2, probeT3, probeT4, probeT5, System.nanoTime());
          }
        }
        case ServerStreamDispatcher s -> {
          DispatchResponseStream out = new DispatchResponseStream(dispatch, callId);
          try {
            s.invoke(instance, call.request(), codec, ctx, out);
            out.complete();
          } catch (Throwable t) {
            out.fail(t);
          }
        }
        case ClientStreamDispatcher c -> {
          DispatchRequestStream in = new DispatchRequestStream(dispatch, callId, call.request());
          byte[] response = c.invoke(instance, in, codec, ctx);
          dispatch.respond(callId, response, null);
        }
        case BidiStreamDispatcher b -> {
          DispatchRequestStream in = new DispatchRequestStream(dispatch, callId, call.request());
          DispatchResponseStream out = new DispatchResponseStream(dispatch, callId);
          try {
            b.invoke(instance, in, codec, ctx, out);
            out.complete();
          } catch (Throwable t) {
            out.fail(t);
          }
        }
      }
    } catch (Throwable t) {
      failWith(callId, t);
    }
  }

  private CallContext buildCallContext(Inbound call, MethodDispatcher method) {
    long callId = call.callId();
    int flags = call.flags();
    Map<String, String> metadata =
        (flags & Dispatch.FLAG_HAS_METADATA) != 0
            ? new HashMap<>(dispatch.metadata(callId))
            : new HashMap<>();
    Map<String, String> attributes = new HashMap<>();
    // A forwarded call runs as the user: the gateway's own attributes are not the caller's.
    Map<String, String> peerAttrs =
        (flags & Dispatch.FLAG_IROH_AUTHENTICATED) != 0 && call.actor() == null
            ? peerAttributes.get(call.transportPeer())
            : null;
    if (peerAttrs != null) {
      attributes.putAll(peerAttrs);
      // Roles are Rust's to decide (an enrolled peer's come from its credential).
      attributes.remove("aster.role");
    }
    if ((flags & Dispatch.FLAG_HAS_ATTRIBUTES) != 0) {
      attributes.putAll(dispatch.attributes(callId));
    }

    // Selected QUIC path of the borrowed connection handle (owned by the server; not freed here).
    java.net.InetSocketAddress peerAddr = null;
    String relayUrl = null;
    java.time.Duration rtt = null;
    if (call.connectionHandle() != 0) {
      try {
        var snap =
            site.aster.handle.IrohConnection.transportSnapshot(
                node.runtime(), call.connectionHandle());
        peerAddr = snap.peerAddr();
        relayUrl = snap.relayUrl();
        rtt = snap.rtt();
      } catch (RuntimeException ignored) {
        // The connection may have closed since.
      }
    }
    site.aster.identity.ForwardedIdentity onBehalfOf =
        call.actor() != null ? dispatch.onBehalfOf(callId) : null;
    var d = method.descriptor();
    return CallContext.builder(call.service().descriptor.name(), d.name())
        .callId(Integer.toString(call.wireCallId()))
        .wireCallId(call.wireCallId())
        .dispatchCallId(callId)
        .peer(call.transportPeer())
        .principal(call.peer())
        .actor(call.actor())
        .onBehalfOf(onBehalfOf)
        .nodeProperties(this::nodeProperties)
        .irohAuthenticated((flags & Dispatch.FLAG_IROH_AUTHENTICATED) != 0)
        .metadata(metadata)
        .attributes(attributes)
        .deadline(call.deadlineUnixMs() > 0 ? call.deadlineUnixMs() / 1000.0 : null)
        .sessionId(call.sessionId() == 0 ? null : Integer.toString(call.sessionId()))
        .streaming(d.streaming().name().endsWith("STREAM"))
        .pattern(d.streaming().name().toLowerCase())
        .idempotent(d.idempotent())
        .peerAddr(peerAddr)
        .relayUrl(relayUrl)
        .rtt(rtt)
        .cancellation(() -> dispatch.isCancelled(callId))
        // A call that ended normally never reports cancelled (Rust CallContext::cancelled).
        .cancelledSignal(
            () -> call.over().get() ? new CompletableFuture<Void>() : cancelSignal(callId))
        .build();
  }

  /** Answer a call with the error {@code t} maps to (a no-op when the call is already over). */
  private void failWith(long callId, Throwable t) {
    if (t instanceof RpcError e) {
      fail(callId, e.code(), e.rpcMessage(), e.details());
    } else if (t instanceof java.util.concurrent.CancellationException) {
      // Dispatch.recv observed the call's (or the handler thread's) cancellation.
      fail(callId, StatusCode.CANCELLED, t.getMessage(), null);
    } else {
      fail(
          callId,
          StatusCode.INTERNAL,
          t.getMessage() == null ? t.getClass().getSimpleName() : t.getMessage(),
          null);
    }
  }

  private void fail(long callId, StatusCode code, String message, Map<String, String> details) {
    try {
      dispatch.respond(callId, null, statusBytes(code, message, details));
    } catch (IrohException ignored) {
      // The call ended under us.
    }
  }

  static byte[] statusBytes(StatusCode code, String message, Map<String, String> details) {
    List<String> keys = new ArrayList<>();
    List<String> values = new ArrayList<>();
    if (details != null) {
      for (Map.Entry<String, String> e : details.entrySet()) {
        keys.add(e.getKey());
        values.add(e.getValue() == null ? "" : e.getValue());
      }
    }
    return AsterEnvelope.encodeRpcStatus(
        new RpcStatus(code.value(), message == null ? "" : message, keys, values));
  }

  private static void closeIfAutoCloseable(Object instance) {
    if (instance instanceof AutoCloseable closeable) {
      try {
        closeable.close();
      } catch (Exception ignored) {
        // close hooks must not break the poll loop
      }
    }
  }

  // ───── Manifest ──────────────────────────────────────────────────────────

  private static List<ServiceDescriptor> buildManifest(
      java.util.Collection<RegisteredService> registered) {
    List<ServiceDescriptor> all = new ArrayList<>();
    for (RegisteredService rs : registered) {
      all.add(rs.descriptor);
    }
    return List.copyOf(all);
  }

  // ───── Builder ───────────────────────────────────────────────────────────

  public static Builder builder() {
    return new Builder();
  }

  public static final class Builder {
    private AsterConfig config;
    private Codec codec;
    private boolean builtinTopology;
    private site.aster.contract.CapabilityRequirement topologyRequirement;
    private final Map<String, RegisteredService> services = new LinkedHashMap<>();
    private List<Interceptor> interceptors = List.of();
    private List<String> alpns = List.of(ASTER_ALPN);
    private site.aster.enrollment.Policy enrollmentPolicy;
    private site.aster.enrollment.RoleMapping enrollmentRoles;
    private site.aster.lease.LeaseGrantor leaseGrantor;
    private int ringCapacity = DEFAULT_RING_CAPACITY;
    private int maxSessionsPerConnection = DEFAULT_MAX_SESSIONS_PER_CONNECTION;
    private site.aster.health.HealthServer health;
    private String healthHost;
    private int healthPort = -1;
    private Boolean acceptOnBehalfOf;
    private site.aster.audit.AuditOutbox.Config auditOutboxConfig;

    private Builder() {}

    /**
     * Accept (or refuse) calls a gateway makes on behalf of a user, overriding the config's {@code
     * [identity] accept_on_behalf_of} (Rust {@code AsterServerBuilder::accept_on_behalf_of}; {@code
     * ASTER_SERVER_OPTION_ACCEPT_ON_BEHALF_OF}). Off, a call carrying {@link
     * site.aster.identity.GatewayIdentity#ON_BEHALF_OF_METADATA} is refused with {@code
     * PERMISSION_DENIED} rather than run as the gateway. {@code null} = the config decides.
     */
    public Builder acceptOnBehalfOf(Boolean accept) {
      this.acceptOnBehalfOf = accept;
      return this;
    }

    /**
     * Record every {@code aster.ops.NodeConfig} set and clear durably (Rust {@code
     * AsterServerBuilder::audit_outbox}; {@code aster_server_set_audit_outbox}): the server opens
     * an audit outbox with {@code config} on its node ({@link AsterServer#auditOutbox()}) and
     * closes it on close. A change the outbox does not accept is refused. Fails the start when the
     * config turns NodeConfig off ({@code [remote_config] enabled = false}).
     */
    public Builder auditOutbox(site.aster.audit.AuditOutbox.Config config) {
      this.auditOutboxConfig = config;
      return this;
    }

    /**
     * Serve the ops endpoints ({@code /healthz}, {@code /readyz}, {@code /metrics}) on {@code
     * host}:{@code port} ({@code port} 0 = ephemeral; read it from {@link AsterServer#health()}).
     * The listener is started with the server's node, attached before start (a successful start
     * sets ready) and stopped by {@link AsterServer#close}. Bind {@code 127.0.0.1} unless a probe
     * outside the host needs it.
     */
    public Builder health(String host, int port) {
      this.healthHost = host;
      this.healthPort = port;
      this.health = null;
      return this;
    }

    /**
     * Attach an ops listener you started yourself ({@link site.aster.health.HealthServer#start}).
     * You close it; the server only counts calls into it and sets ready / not ready.
     */
    public Builder health(site.aster.health.HealthServer health) {
      this.health = health;
      this.healthPort = -1;
      return this;
    }

    /**
     * The node and Enrollment configuration (usually {@link AsterConfig#fromEnv()} or {@link
     * AsterConfig#fromFile}). With enrollment configured the server is guarded by Enrollment and
     * each peer's credential roles reach {@code @Requires}.
     */
    public Builder config(AsterConfig config) {
      this.config = config;
      return this;
    }

    /**
     * Judge peers' enrollment evidence with {@code policy} instead of the configured {@code
     * [enrollment] policy} (Rust {@code AsterServerBuilder::enrollment_policy}). Enablement still
     * comes from the config.
     */
    public Builder enrollmentPolicy(site.aster.enrollment.Policy policy) {
      this.enrollmentPolicy = policy;
      return this;
    }

    /**
     * Map verified claims to roles with {@code roles} instead of the configured {@code
     * [enrollment.roles]} (Rust {@code AsterServerBuilder::enrollment_roles}).
     */
    public Builder enrollmentRoles(site.aster.enrollment.RoleMapping roles) {
      this.enrollmentRoles = roles;
      return this;
    }

    /**
     * Serve {@code aster.lease.Grantor} (Rust {@code aster::rpc::lease::LeaseGrantor}): the
     * designated serialization point other nodes acquire leases through ({@code
     * AsterClient.leaseGrantor}).
     */
    public Builder leaseGrantor(site.aster.lease.LeaseGrantor grantor) {
      this.leaseGrantor = grantor;
      return this;
    }

    /** Explicitly enable/disable aster.net.Topology; disabled by default. Requires monitoring. */
    public Builder builtinTopology(boolean enabled) {
      builtinTopology = enabled;
      return this;
    }

    /** Optional Rust-enforced role policy for every topology RPC method. */
    public Builder topologyRequires(site.aster.contract.CapabilityRequirement requirement) {
      topologyRequirement = requirement;
      return this;
    }

    public Builder codec(Codec codec) {
      this.codec = codec;
      return this;
    }

    public Builder interceptors(List<Interceptor> interceptors) {
      this.interceptors = List.copyOf(interceptors);
      return this;
    }

    public Builder alpns(List<String> extraAlpns) {
      var all = new ArrayList<>(List.of(ASTER_ALPN));
      for (String a : extraAlpns) {
        if (!a.equals(ASTER_ALPN)) {
          all.add(a);
        }
      }
      this.alpns = List.copyOf(all);
      return this;
    }

    public Builder ringCapacity(int capacity) {
      this.ringCapacity = capacity;
      return this;
    }

    /**
     * Maximum number of active sessions per inbound QUIC connection (spec §7.5 / §9). When the cap
     * is reached, Rust refuses further session-create calls from that connection with {@code
     * RESOURCE_EXHAUSTED}. Default {@link #DEFAULT_MAX_SESSIONS_PER_CONNECTION}.
     */
    public Builder maxSessionsPerConnection(int max) {
      if (max < 1) {
        throw new IllegalArgumentException("maxSessionsPerConnection must be >= 1, got " + max);
      }
      this.maxSessionsPerConnection = max;
      return this;
    }

    /** Register a SHARED-scope service instance (its generated dispatcher must be on the path). */
    public Builder service(Object instance) {
      return service(instance, findDispatcherFor(instance.getClass()));
    }

    /** Register a SHARED-scope service instance with an explicit {@link ServiceDispatcher}. */
    public Builder service(Object instance, ServiceDispatcher dispatcher) {
      if (dispatcher.descriptor().scope() != Scope.SHARED) {
        throw new IllegalArgumentException(
            "service() requires a SHARED-scope service; use sessionService() for "
                + dispatcher.descriptor().scope());
      }
      services.put(
          dispatcher.descriptor().name(), RegisteredService.of(dispatcher, instance, null));
      return this;
    }

    /** Register a SESSION-scope service class with a per-session factory (given the peer id). */
    public Builder sessionService(Class<?> implClass, Function<String, Object> factory) {
      return sessionService(implClass, factory, findDispatcherFor(implClass));
    }

    /** Register a SESSION-scope service with an explicit {@link ServiceDispatcher}. */
    public Builder sessionService(
        Class<?> implClass, Function<String, Object> factory, ServiceDispatcher dispatcher) {
      if (dispatcher.descriptor().scope() != Scope.SESSION) {
        throw new IllegalArgumentException(
            "sessionService() requires a SESSION-scope service; got "
                + dispatcher.descriptor().scope());
      }
      services.put(dispatcher.descriptor().name(), RegisteredService.of(dispatcher, null, factory));
      return this;
    }

    private static ServiceDispatcher findDispatcherFor(Class<?> implClass) {
      for (ServiceDispatcher d : ServiceLoader.load(ServiceDispatcher.class)) {
        if (d.descriptor().implClass().equals(implClass)) {
          return d;
        }
      }
      throw new IllegalStateException(
          "No generated ServiceDispatcher found for "
              + implClass.getName()
              + " — did you run the annotation processor (aster-codegen-apt / aster-codegen-ksp)?");
    }

    public CompletableFuture<AsterServer> build() {
      if (builtinTopology && (config == null || !config.enableMonitoring()))
        return CompletableFuture.failedFuture(
            new IllegalArgumentException("builtinTopology requires monitoring"));
      Observability
          .ensureStarted(); // opens the log bridge (design Q2); exporters only if OTEL_* asks
      List<byte[]> alpnBytes = alpns.stream().map(s -> s.getBytes(StandardCharsets.UTF_8)).toList();
      byte[] trust;
      try {
        trust =
            site.aster.enrollment.TrustJson.encode(
                config == null ? AsterConfig.builder().build() : config,
                enrollmentPolicy,
                enrollmentRoles);
      } catch (RuntimeException e) {
        return CompletableFuture.failedFuture(e);
      }
      // aster_node_start_enrolled (ABI 5.1): validates and installs Enrollment when the config
      // enables it (failing closed), else a plain aster_node_start; plus the RPC ALPN.
      CompletableFuture<IrohNode> nodeFuture =
          IrohNode.startEnrolled(
              config, alpnBytes, trust, site.aster.ffi.NativeConfig.ENROLLMENT_SIDE_SERVER);
      Builder self = this;
      return nodeFuture
          .thenApply(
              node -> {
                try {
                  return self.serve(node);
                } catch (RuntimeException e) {
                  node.close();
                  throw e instanceof IrohException ie
                      ? IrohException.forStatus(
                          ie.status(), "Failed to start the server: " + e.getMessage())
                      : new IrohException("Failed to start the server: " + e.getMessage());
                }
              })
          .thenCompose(
              server -> {
                // Publish each service's contract collection into a fresh registry doc (Python's
                // AsterServer._publish_contracts). Non-fatal.
                List<ServiceDispatcher> dispatchers = new ArrayList<>();
                for (RegisteredService rs : server.services.values()) {
                  dispatchers.add(rs.dispatcher);
                }
                if (dispatchers.isEmpty()) {
                  return CompletableFuture.completedFuture(server);
                }
                return site.aster.contract.ContractPublisher.publishAll(server.node, dispatchers)
                    .handle(
                        (published, err) -> {
                          if (err == null && published != null) {
                            server.setPublicationResult(
                                published.registryNamespace(), published.contractIds());
                          } else if (err != null) {
                            System.err.println(
                                "AsterServer: contract publication failed (non-fatal): " + err);
                          }
                          return server;
                        });
              });
    }

    /** Register every service with Rust and start serving on {@code node}. */
    private AsterServer serve(IrohNode node) {
      long rt = node.runtime().nativeHandle();
      RegisteredService[] byIndex = new RegisteredService[services.size()];
      try (Dispatch.Builder b = new Dispatch.Builder(rt, node.nodeHandle())) {
        for (RegisteredService rs : services.values()) {
          int idx = b.addServiceJson(rs.spec.json());
          byIndex[idx] = rs;
        }
        b.option(Dispatch.OPTION_MAX_SESSIONS_PER_CONNECTION, maxSessionsPerConnection);
        if (acceptOnBehalfOf != null) {
          b.option(Dispatch.OPTION_ACCEPT_ON_BEHALF_OF, acceptOnBehalfOf ? 1 : 0);
        }
        b.topology(builtinTopology, topologyRequirement);
        site.aster.health.HealthServer attached = health;
        boolean owned = false;
        if (attached == null && healthPort >= 0) {
          attached = site.aster.health.HealthServer.start(node, healthHost, healthPort);
          owned = true;
        } else if (attached == null && config != null && config.isHealthEnabled()) {
          // Explicit builder health(...) wins; otherwise the config (ASTER_HEALTH_*, [health]).
          attached =
              site.aster.health.HealthServer.start(node, config.healthHost(), config.healthPort());
          owned = true;
        }
        site.aster.lease.LeaseGrantor.Backend grantor = null;
        site.aster.audit.AuditOutbox outbox = null;
        try {
          if (auditOutboxConfig != null) {
            try {
              outbox =
                  site.aster.audit.AuditOutbox.open(node.runtime(), auditOutboxConfig)
                      .get(30, TimeUnit.SECONDS);
            } catch (java.util.concurrent.ExecutionException e) {
              throw e.getCause() instanceof RuntimeException re
                  ? re
                  : new IrohException("audit outbox: " + e.getCause());
            } catch (Exception e) {
              throw new IrohException("audit outbox: " + e);
            }
            b.auditOutbox(outbox.nativeHandle());
          }
          if (attached != null) {
            b.attachHealth(attached.nativeHandle());
          }
          if (leaseGrantor != null) {
            grantor = leaseGrantor.start(node.runtime());
            b.leaseGrantor(
                grantor.serializer().nativeHandle(),
                leaseGrantor.requiresJson(),
                leaseGrantor.revokeRequiresJson());
          }
          // With enrollment and replicas this blocks up to sync_timeout_ms (policy sync).
          Dispatch dispatch = b.start(ringCapacity);
          return new AsterServer(this, node, dispatch, byIndex, attached, owned, grantor, outbox);
        } catch (RuntimeException e) {
          if (grantor != null) {
            grantor.close();
          }
          if (outbox != null) {
            outbox.close();
          }
          if (owned) {
            attached.close();
          }
          throw e;
        }
      }
    }
  }

  /**
   * A dispatcher bound to its instance (SHARED) or factory (SESSION), with its JSON registration
   * form and its method dispatchers in {@code method_idx} order.
   */
  private record RegisteredService(
      ServiceDescriptor descriptor,
      ServiceDispatcher dispatcher,
      Object sharedInstance,
      Function<String, Object> factory,
      ContractManifestBuilder.DynamicServiceSpec spec,
      MethodDispatcher[] methods,
      String contractId) {

    static RegisteredService of(
        ServiceDispatcher dispatcher, Object instance, Function<String, Object> factory) {
      ContractManifestBuilder.DynamicServiceSpec spec =
          ContractManifestBuilder.dynamicService(dispatcher);
      Map<String, MethodDispatcher> byName = dispatcher.methods();
      MethodDispatcher[] methods = new MethodDispatcher[spec.methodNames().size()];
      for (int i = 0; i < methods.length; i++) {
        methods[i] = byName.get(spec.methodNames().get(i));
      }
      return new RegisteredService(
          dispatcher.descriptor(), dispatcher, instance, factory, spec, methods, spec.contractId());
    }
  }
}
