package site.aster.server;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
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
 */
public final class AsterServer implements AutoCloseable {

  public static final String ASTER_ALPN = "aster/1";
  public static final String CONSUMER_ADMISSION_ALPN = "aster.consumer_admission";
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
  private final Thread pollThread;
  private final ExecutorService callExecutor;
  private final ExecutorService streamingExecutor;
  private final site.aster.trust.ConsumerAdmissionHandler admissionHandler;

  /** The ops listener attached to this server (null = none); closed with the server if owned. */
  private final site.aster.health.HealthServer health;

  private final boolean ownsHealth;

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

  private AsterServer(
      Builder b,
      IrohNode node,
      Dispatch dispatch,
      RegisteredService[] byIndex,
      site.aster.health.HealthServer health,
      boolean ownsHealth) {
    this.node = node;
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
    // Consumer admission (open-gate): serves the service summaries on the
    // aster.consumer_admission ALPN, which Rust hands over as NON_RPC events.
    this.admissionHandler =
        new site.aster.trust.ConsumerAdmissionHandler(
            this::buildServiceSummaries,
            () -> registryNamespace,
            this::setPeerAttributes,
            () -> this.config == null || this.config.allowAllConsumers());
    this.pollThread =
        Thread.ofPlatform().daemon(true).name("aster-server-poll").start(this::pollLoop);
  }

  private List<site.aster.registry.ServiceSummary> buildServiceSummaries() {
    // channels["rpc"] = this node's address, so consumers dial aster/1 directly (Python's
    // ServiceSummary in runtime.py does the same).
    String rpcAddr = node.nodeAddr().toBase64();
    List<site.aster.registry.ServiceSummary> out = new ArrayList<>();
    for (RegisteredService rs : services.values()) {
      var summary = new site.aster.registry.ServiceSummary();
      summary.name = rs.descriptor.name();
      summary.version = rs.descriptor.version();
      String cid = publishedContractIds.get(rs.descriptor.name());
      if (cid == null || cid.isEmpty()) {
        cid = rs.contractId;
      }
      summary.contractId = cid == null ? "" : cid;
      summary.pattern = rs.descriptor.scope() == Scope.SHARED ? "shared" : "session";
      summary.serializationModes = List.of("xlang");
      summary.channels.put("rpc", rpcAddr);
      out.add(summary);
    }
    return List.copyOf(out);
  }

  /** 64-char hex doc-id of the registry doc hosting this server's manifests; empty until set. */
  public String registryNamespace() {
    return registryNamespace;
  }

  void setPublicationResult(String namespace, Map<String, String> contractIds) {
    this.registryNamespace = namespace == null ? "" : namespace;
    this.publishedContractIds.putAll(contractIds);
  }

  /** The node ID of this server (hex string). */
  public String nodeId() {
    return node.nodeId();
  }

  /** The ops listener attached via {@link Builder#health}, or {@code null}. */
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
    peerAttributes.put(peerId, Map.copyOf(attributes));
    dispatch.setPeerAttributes(peerId, attributes);
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
    admissionHandler.close();
    for (Object instance : sessions.values()) {
      closeIfAutoCloseable(instance);
    }
    sessions.clear();
    connections.clear();
    dispatch.close(); // aster_server_destroy clears the attached listener's ready flag first
    if (ownsHealth && health != null) {
      health.close();
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
      long connectionHandle) {}

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
        // cancel); handlers see it through isCancelled() and GONE results.
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
    byte[] request = bytes(ev, Dispatch.OFFSET_REQUEST_PTR, Dispatch.OFFSET_REQUEST_LEN);
    String peer =
        new String(
            bytes(ev, Dispatch.OFFSET_PEER_PTR, Dispatch.OFFSET_PEER_LEN), StandardCharsets.UTF_8);
    dispatch.bufferRelease(ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_BUFFER));

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
            connectionHandle);
    callExecutor.execute(() -> dispatchCall(call));
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
    byte[] alpn = bytes(ev, Dispatch.OFFSET_REQUEST_PTR, Dispatch.OFFSET_REQUEST_LEN);
    dispatch.bufferRelease(ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_BUFFER));
    long handle = ev.get(ValueLayout.JAVA_LONG, Dispatch.OFFSET_CONNECTION_HANDLE);
    // Owned by us: the wrapper frees it.
    site.aster.handle.IrohConnection conn =
        new site.aster.handle.IrohConnection(node.runtime(), handle);
    if (site.aster.trust.ConsumerAdmissionHandler.matches(alpn)) {
      admissionHandler.onConnection(conn);
    } else {
      conn.close();
    }
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
      failWith(callId, t);
    }
  }

  private void runDispatch(
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
    Map<String, String> peerAttrs = peerAttributes.get(call.peer());
    if (peerAttrs != null) {
      attributes.putAll(peerAttrs);
    }
    if ((flags & Dispatch.FLAG_HAS_ATTRIBUTES) != 0 && peerAttrs == null) {
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
    var d = method.descriptor();
    return CallContext.builder(call.service().descriptor.name(), d.name())
        .peer(call.peer())
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
    private final Map<String, RegisteredService> services = new LinkedHashMap<>();
    private List<Interceptor> interceptors = List.of();
    private List<String> alpns = List.of(ASTER_ALPN, CONSUMER_ADMISSION_ALPN);
    private int ringCapacity = DEFAULT_RING_CAPACITY;
    private int maxSessionsPerConnection = DEFAULT_MAX_SESSIONS_PER_CONNECTION;
    private site.aster.health.HealthServer health;
    private String healthHost;
    private int healthPort = -1;

    private Builder() {}

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

    public Builder config(AsterConfig config) {
      this.config = config;
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
      var all = new ArrayList<>(List.of(ASTER_ALPN, CONSUMER_ADMISSION_ALPN));
      for (String a : extraAlpns) {
        if (!a.equals(ASTER_ALPN) && !a.equals(CONSUMER_ADMISSION_ALPN)) {
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
      Observability
          .ensureStarted(); // opens the log bridge (design Q2); exporters only if OTEL_* asks
      List<byte[]> alpnBytes = alpns.stream().map(s -> s.getBytes(StandardCharsets.UTF_8)).toList();
      // aster_node_start with the full config (ABI 3.2), plus the RPC + admission ALPNs.
      CompletableFuture<IrohNode> nodeFuture = IrohNode.start(config, alpnBytes, null);
      Builder self = this;
      return nodeFuture
          .thenApply(
              node -> {
                try {
                  return self.serve(node);
                } catch (RuntimeException e) {
                  node.close();
                  throw new IrohException("Failed to start the dispatch server: " + e.getMessage());
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
        site.aster.health.HealthServer attached = health;
        boolean owned = false;
        if (attached == null && healthPort >= 0) {
          attached = site.aster.health.HealthServer.start(node, healthHost, healthPort);
          owned = true;
        }
        try {
          if (attached != null) {
            b.attachHealth(attached.nativeHandle());
          }
          Dispatch dispatch = b.start(ringCapacity);
          return new AsterServer(this, node, dispatch, byIndex, attached, owned);
        } catch (RuntimeException e) {
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
