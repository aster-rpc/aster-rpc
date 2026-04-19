package site.aster.server;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.ServiceLoader;
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
import site.aster.ffi.IrohException;
import site.aster.ffi.Reactor;
import site.aster.interceptors.CallContext;
import site.aster.interceptors.Interceptor;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.IrohNode;
import site.aster.server.session.InMemorySessionRegistry;
import site.aster.server.session.SessionKey;
import site.aster.server.session.SessionRegistry;
import site.aster.server.spi.BidiStreamDispatcher;
import site.aster.server.spi.ClientStreamDispatcher;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.ServerStreamDispatcher;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.UnaryDispatcher;
import site.aster.server.wire.CallHeader;
import site.aster.server.wire.RpcStatus;
import site.aster.server.wire.StreamHeader;

/**
 * High-level Aster RPC server. Discovers {@link ServiceDispatcher}s via {@link ServiceLoader},
 * binds each to a user-supplied instance (or per-session factory), decodes incoming {@link
 * StreamHeader} frames from the reactor's poll loop, resolves the right method dispatcher, runs the
 * interceptor chain, and dispatches by sealed {@link MethodDispatcher} subtype.
 *
 * <p>Manifest submission (via {@code RegistryAsync.publishAsync}) is not yet hooked; the manifest
 * is built at startup and exposed via {@link #manifest()} but not actually published. Hook dispatch
 * ({@code IROH_EVENT_HOOK_*} → user callbacks) is likewise deferred.
 */
public final class AsterServer implements AutoCloseable {

  public static final String ASTER_ALPN = "aster/1";
  public static final String CONSUMER_ADMISSION_ALPN = "aster.consumer_admission";
  private static final int DEFAULT_RING_CAPACITY = 256;
  private static final int DEFAULT_POLL_BATCH = 32;
  private static final int POLL_TIMEOUT_MS = 100;

  /**
   * Default per-connection session cap (spec §7.5 / §9 — {@code
   * aster.transport.max_sessions_per_connection}). Sized to be generous for normal use but small
   * enough that a single connection cannot OOM the server. Configurable via {@link
   * Builder#maxSessionsPerConnection}.
   */
  static final int DEFAULT_MAX_SESSIONS_PER_CONNECTION = 1024;

  private final IrohNode node;
  private final Reactor reactor;
  private final Codec codec;
  private final ForyCodec foryHeaderCodec;
  private volatile byte[] okTrailerBytesCache;
  private final Map<String, RegisteredService> services;
  private final SessionRegistry sessionRegistry;
  private final List<Interceptor> interceptors;
  private final AsterConfig config;
  private final int maxSessionsPerConnection;
  private final List<ServiceDescriptor> manifest;
  private final AtomicBoolean running = new AtomicBoolean(true);
  private final Thread pollThread;
  private final ExecutorService callExecutor;
  private final ExecutorService streamingExecutor;
  private final site.aster.trust.ConsumerAdmissionHandler admissionHandler;
  private final Thread nonRpcAcceptThread;

  /**
   * 64-char hex doc-id of the registry doc created by {@link
   * site.aster.contract.ContractPublisher}, advertised in consumer-admission responses. Empty until
   * {@link Builder#build()}'s post-construction publish step completes; blank-supplier behaviour
   * otherwise.
   */
  private volatile String registryNamespace = "";

  /** Populated by {@link site.aster.contract.ContractPublisher} after publication. */
  private final Map<String, String> publishedContractIds = new ConcurrentHashMap<>();

  /**
   * Per-connection session state (spec §7.5). Mutated only from the poll thread; the dispatcher
   * threads read the resolved session instance via {@link SessionRegistry} so they don't need to
   * touch this map. Concurrent-hash-map is overkill for the current single-poll-thread design but
   * costs us nothing and keeps the door open for multi-poller futures.
   */
  private final Map<Long, ConnectionState> connections = new ConcurrentHashMap<>();

  /**
   * Per-peer admission attributes merged into every {@link CallContext#attributes()} built for
   * calls from that peer. Populated by the admission / auth layer (dev mode leaves it empty,
   * auth-mode populates {@code aster.role} from validated credentials); consumed by the {@link
   * site.aster.interceptors.CapabilityInterceptor}. Key is {@code AsterCall#peerId()}, which is
   * stable for the lifetime of a QUIC connection.
   */
  private final Map<String, Map<String, String>> peerAttributes = new ConcurrentHashMap<>();

  private AsterServer(Builder b, IrohNode node, Reactor reactor) {
    this.node = node;
    this.reactor = reactor;
    this.codec = b.codec != null ? b.codec : new ForyCodec();
    this.foryHeaderCodec = registerFrameworkWireTypes(this.codec);
    this.services = Map.copyOf(b.services);
    this.sessionRegistry = b.sessionRegistry;
    this.interceptors = List.copyOf(b.interceptors);
    this.config = b.config;
    this.maxSessionsPerConnection = b.maxSessionsPerConnection;
    this.manifest = buildManifest(this.services.values());
    this.callExecutor = Executors.newVirtualThreadPerTaskExecutor();
    // Streaming dispatchers (client-stream / bidi) call ReactorRequestStream.receive ->
    // Reactor.recvFrame -> runtime.block_on(rx.recv()). block_on parks the calling thread,
    // and on a virtual thread that pins the carrier for up to ~POLL_TIMEOUT_MS per recv.
    // Hop streaming dispatchers to a platform-thread executor instead so the carriers stay
    // free for unary / server-stream work running on the VT executor.
    this.streamingExecutor =
        Executors.newCachedThreadPool(
            r -> {
              Thread t = new Thread(r, "aster-server-streaming");
              t.setDaemon(true);
              return t;
            });
    this.pollThread =
        Thread.ofPlatform().daemon(true).name("aster-server-poll").start(this::pollLoop);
    // Consumer admission (open-gate): serve the service summaries for every
    // consumer that opens the aster.consumer_admission ALPN. Auth-mode credential
    // verification is tracked as tasks #14/#15.
    this.admissionHandler =
        new site.aster.trust.ConsumerAdmissionHandler(
            this::buildServiceSummaries, () -> registryNamespace);
    this.nonRpcAcceptThread =
        Thread.ofPlatform()
            .daemon(true)
            .name("aster-server-non-rpc-accept")
            .start(this::nonRpcAcceptLoop);
  }

  /**
   * Drives the reactor's non-rpc accept queue: for each aster-ALPN connection negotiated with an
   * ALPN other than {@code aster/1}, dispatch to the appropriate per-ALPN handler. This is the
   * Java-side mirror of Python's {@code _accept_loop} in {@code runtime.py} (lines 837-894), scoped
   * to the ALPNs Java currently supports (consumer admission today; producer admission / manifest
   * serving are follow-up tasks).
   */
  private void nonRpcAcceptLoop() {
    site.aster.ffi.IrohLibrary lib = site.aster.ffi.IrohLibrary.getInstance();
    long runtimeHandle = node.runtime().nativeHandle();
    long reactorHandle = reactor.handle();
    while (running.get()) {
      try (java.lang.foreign.Arena arena = java.lang.foreign.Arena.ofConfined()) {
        java.lang.foreign.MemorySegment opSeg =
            arena.allocate(java.lang.foreign.ValueLayout.JAVA_LONG);
        int status = lib.asterReactorAcceptNonRpc(runtimeHandle, reactorHandle, 0L, opSeg);
        if (status != 0) {
          // Reactor shut down or error — exit quietly.
          return;
        }
        long opId = opSeg.get(java.lang.foreign.ValueLayout.JAVA_LONG, 0);
        java.util.concurrent.CompletableFuture<site.aster.event.IrohEvent> fut =
            node.runtime().registry().register(opId);
        site.aster.event.IrohEvent event;
        try {
          event = fut.get();
        } catch (Exception e) {
          if (!running.get()) {
            return;
          }
          continue;
        }
        if (event.kind() != site.aster.ffi.IrohEventKind.ASTER_ACCEPTED) {
          continue;
        }
        long connHandle = event.handle();
        byte[] alpnBytes = new byte[0];
        if (event.dataLen() > 0) {
          alpnBytes =
              event
                  .data()
                  .asSlice(0, event.dataLen())
                  .toArray(java.lang.foreign.ValueLayout.JAVA_BYTE);
          if (event.hasBuffer()) {
            node.runtime().releaseBuffer(event.buffer());
          }
        }
        site.aster.handle.IrohConnection conn =
            new site.aster.handle.IrohConnection(node.runtime(), connHandle);
        if (site.aster.trust.ConsumerAdmissionHandler.matches(alpnBytes)) {
          admissionHandler.onConnection(conn);
        } else {
          conn.close();
        }
      } catch (Exception e) {
        if (!running.get()) {
          return;
        }
      }
    }
  }

  private List<site.aster.registry.ServiceSummary> buildServiceSummaries() {
    // Populate channels["rpc"] with the server's own node addr (base64). Consumers use this
    // to dial the RPC endpoint via the aster/1 ALPN without going through the admission peer
    // again — mirrors Python's ServiceSummary population in runtime.py:397.
    String rpcAddr = node.nodeAddr().toBase64();
    List<site.aster.registry.ServiceSummary> out = new ArrayList<>();
    for (RegisteredService rs : services.values()) {
      var summary = new site.aster.registry.ServiceSummary();
      summary.name = rs.descriptor.name();
      summary.version = rs.descriptor.version();
      // Prefer the cached contract_id populated by ContractPublisher. Fall back to computing it
      // on demand when publication is disabled or hasn't completed (e.g. tests that instantiate
      // AsterServer without going through the public Builder.build() path).
      String cid = publishedContractIds.get(rs.descriptor.name());
      if (cid == null || cid.isEmpty()) {
        cid = contractId(rs.descriptor.name());
      }
      summary.contractId = cid == null ? "" : cid;
      summary.pattern = rs.descriptor.scope() == Scope.SHARED ? "shared" : "session";
      summary.serializationModes = List.of("xlang");
      summary.channels.put("rpc", rpcAddr);
      out.add(summary);
    }
    return List.copyOf(out);
  }

  /**
   * 64-char hex doc-id of the registry doc that hosts this server's published contract manifests.
   * Empty until {@link Builder#build()} finishes its publication step, or when the server was
   * instantiated without publication. Consumers use this (via the admission response) to {@code
   * join_and_subscribe_namespace} and fetch manifests by {@code contract_id}.
   */
  public String registryNamespace() {
    return registryNamespace;
  }

  void setPublicationResult(String namespace, java.util.Map<String, String> contractIds) {
    this.registryNamespace = namespace == null ? "" : namespace;
    this.publishedContractIds.putAll(contractIds);
  }

  /** The node ID of this server (hex string). */
  public String nodeId() {
    return node.nodeId();
  }

  public IrohNode node() {
    return node;
  }

  public AsterConfig config() {
    return config;
  }

  /** Immutable snapshot of the discovered service descriptors — one per registered service. */
  /**
   * <strong>TEST-ONLY</strong>. Snapshot of per-connection state for assertions in tier-2 chaos
   * tests. Maps {@code connectionId} to {@code (activeSessionCount, lastOpenedSessionId)}.
   * Production code MUST NOT read this — it exists so tests can verify reap semantics (connection
   * entries dropped on close, sessions counted correctly) without reflecting into private fields.
   * Mirrors the TypeScript {@code AsterServer2.debugConnectionSnapshot}.
   */
  public Map<Long, ConnectionSnapshot> debugConnectionSnapshot() {
    Map<Long, ConnectionSnapshot> out = new java.util.HashMap<>();
    for (Map.Entry<Long, ConnectionState> e : connections.entrySet()) {
      ConnectionState state = e.getValue();
      synchronized (state) {
        out.put(
            e.getKey(),
            new ConnectionSnapshot(state.activeSessions.size(), state.lastOpenedSessionId));
      }
    }
    return out;
  }

  /** Test-only snapshot record; see {@link #debugConnectionSnapshot()}. */
  public record ConnectionSnapshot(int activeSessionCount, int lastOpenedSessionId) {}

  public List<ServiceDescriptor> manifest() {
    return manifest;
  }

  /**
   * Live view of the registered {@link ServiceDispatcher}s keyed by service name. Intended for
   * wiring {@link site.aster.interceptors.CapabilityInterceptor} so it can look up the service- and
   * method-level {@code @Requires} at call time without each caller maintaining its own map.
   */
  public Map<String, site.aster.server.spi.ServiceDispatcher> serviceDispatchers() {
    java.util.LinkedHashMap<String, site.aster.server.spi.ServiceDispatcher> out =
        new java.util.LinkedHashMap<>();
    for (Map.Entry<String, RegisteredService> e : services.entrySet()) {
      out.put(e.getKey(), e.getValue().dispatcher());
    }
    return java.util.Collections.unmodifiableMap(out);
  }

  /**
   * Attach admission attributes (e.g. {@code aster.role} → comma-separated capability list) to
   * every subsequent call from {@code peerId}. Called by the auth / admission layer after a peer
   * passes credential validation; merged into {@link CallContext#attributes()} before interceptors
   * run. Pass {@code null} or an empty map to clear.
   */
  public void setPeerAttributes(String peerId, Map<String, String> attributes) {
    if (peerId == null) {
      return;
    }
    if (attributes == null || attributes.isEmpty()) {
      peerAttributes.remove(peerId);
      return;
    }
    peerAttributes.put(peerId, Map.copyOf(attributes));
  }

  /** Drop any cached admission attributes for {@code peerId}. */
  public void clearPeerAttributes(String peerId) {
    if (peerId != null) {
      peerAttributes.remove(peerId);
    }
  }

  /**
   * Build and return the canonical {@link site.aster.contract.ContractManifest} JSON for one
   * registered service. Walks the service's type graph, computes each TypeDef's BLAKE3 hash via the
   * Rust FFI, derives {@code contract_id} via the Rust canonicalizer, and assembles the full
   * manifest (methods, field schemas, metadata).
   *
   * <p>Returns {@code null} if no service with {@code serviceName} is registered on this server.
   */
  public String manifestJson(String serviceName) {
    RegisteredService rs = services.get(serviceName);
    if (rs == null) {
      return null;
    }
    return site.aster.contract.ContractManifestBuilder.build(rs.dispatcher()).toJson();
  }

  /** Compute only the {@code contract_id} for a registered service; {@code null} if unknown. */
  public String contractId(String serviceName) {
    RegisteredService rs = services.get(serviceName);
    if (rs == null) {
      return null;
    }
    return site.aster.contract.ContractManifestBuilder.computeContractId(rs.dispatcher());
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
    sessionRegistry.clear();
    connections.clear();
    reactor.close();
    node.close();
  }

  // ───── Poll loop ─────────────────────────────────────────────────────────

  private void pollLoop() {
    long callSize = Reactor.CALL_LAYOUT.byteSize();
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment callBuffer = arena.allocate(Reactor.CALL_LAYOUT, DEFAULT_POLL_BATCH);
      while (running.get()) {
        int count = reactor.poll(callBuffer, DEFAULT_POLL_BATCH, POLL_TIMEOUT_MS);
        for (int i = 0; i < count; i++) {
          MemorySegment slot = callBuffer.asSlice(i * callSize, callSize);
          byte eventKind = slot.get(ValueLayout.JAVA_BYTE, Reactor.OFFSET_EVENT_KIND);
          if (eventKind == Reactor.EVENT_KIND_CONNECTION_CLOSED) {
            handleConnectionClosed(slot);
            continue;
          }
          AsterCall call = extractCall(slot);
          long headerBuf = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_HEADER_BUFFER);
          long requestBuf = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_REQUEST_BUFFER);
          long peerBuf = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_PEER_BUFFER);
          reactor.bufferRelease(headerBuf);
          reactor.bufferRelease(requestBuf);
          reactor.bufferRelease(peerBuf);
          callExecutor.execute(() -> dispatchCall(call));
        }
      }
    }
  }

  /**
   * Reap per-connection state on connection close (spec §7.5). Drops the session graveyard +
   * counter for this {@code connectionId}; the {@link SessionRegistry} drops every session instance
   * keyed on it. Runs on the poll thread.
   */
  private void handleConnectionClosed(MemorySegment slot) {
    long connectionId = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_CONNECTION_ID);
    long peerBuf = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_PEER_BUFFER);
    reactor.bufferRelease(peerBuf);
    connections.remove(connectionId);
    sessionRegistry.onConnectionClosed(connectionId);
  }

  private AsterCall extractCall(MemorySegment slot) {
    long callId = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_CALL_ID);
    long connectionId = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_CONNECTION_ID);
    long streamId = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_STREAM_ID);
    MemorySegment headerPtr = slot.get(ValueLayout.ADDRESS, Reactor.OFFSET_HEADER_PTR);
    int headerLen = slot.get(ValueLayout.JAVA_INT, Reactor.OFFSET_HEADER_LEN);
    byte headerFlags = slot.get(ValueLayout.JAVA_BYTE, Reactor.OFFSET_HEADER_FLAGS);
    MemorySegment requestPtr = slot.get(ValueLayout.ADDRESS, Reactor.OFFSET_REQUEST_PTR);
    int requestLen = slot.get(ValueLayout.JAVA_INT, Reactor.OFFSET_REQUEST_LEN);
    byte requestFlags = slot.get(ValueLayout.JAVA_BYTE, Reactor.OFFSET_REQUEST_FLAGS);
    MemorySegment peerPtr = slot.get(ValueLayout.ADDRESS, Reactor.OFFSET_PEER_PTR);
    int peerLen = slot.get(ValueLayout.JAVA_INT, Reactor.OFFSET_PEER_LEN);

    byte[] header =
        headerLen > 0
            ? headerPtr.reinterpret(headerLen).toArray(ValueLayout.JAVA_BYTE)
            : new byte[0];
    byte[] request =
        requestLen > 0
            ? requestPtr.reinterpret(requestLen).toArray(ValueLayout.JAVA_BYTE)
            : new byte[0];
    String peerId =
        peerLen > 0
            ? new String(
                peerPtr.reinterpret(peerLen).toArray(ValueLayout.JAVA_BYTE), StandardCharsets.UTF_8)
            : "";
    return new AsterCall(
        callId, connectionId, streamId, header, headerFlags, request, requestFlags, peerId);
  }

  // ───── Dispatch ──────────────────────────────────────────────────────────

  private void dispatchCall(AsterCall call) {
    long callId = call.callId();
    long probeT2 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;
    try {
      StreamHeader header = decodeStreamHeader(call.header());
      long probeT3 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;
      RegisteredService svc = services.get(header.service());
      if (svc == null) {
        submitErrorTrailer(
            callId, StatusCode.UNIMPLEMENTED, "unknown service: " + header.service());
        return;
      }
      MethodDispatcher method = svc.dispatcher.methods().get(header.method());
      if (method == null) {
        submitErrorTrailer(
            callId,
            StatusCode.UNIMPLEMENTED,
            "unknown method: " + header.service() + "/" + header.method());
        return;
      }
      Object instance;
      try {
        instance = resolveInstance(svc, call, header);
      } catch (SessionNotFoundException e) {
        submitErrorTrailer(callId, StatusCode.NOT_FOUND, e.getMessage());
        return;
      } catch (SessionLimitException e) {
        submitErrorTrailer(callId, StatusCode.RESOURCE_EXHAUSTED, e.getMessage());
        return;
      } catch (SessionScopeMismatchException e) {
        submitErrorTrailer(callId, StatusCode.FAILED_PRECONDITION, e.getMessage());
        return;
      }
      CallContext ctx = buildCallContext(header, method, call);

      // Trampoline streaming dispatchers (client-stream / bidi) onto a platform-thread
      // executor so their blocking Reactor.recvFrame calls don't pin a virtual-thread
      // carrier. Unary + server-stream stay on the VT executor (they don't block on
      // recvFrame and benefit from cheap VT fanout).
      if (Thread.currentThread().isVirtual()
          && (method instanceof ClientStreamDispatcher || method instanceof BidiStreamDispatcher)) {
        final MethodDispatcher pinnedMethod = method;
        final Object pinnedInstance = instance;
        final CallContext pinnedCtx = ctx;
        streamingExecutor.execute(
            () -> runDispatch(call, pinnedMethod, pinnedInstance, pinnedCtx, probeT2, probeT3));
        return;
      }

      runDispatch(call, method, instance, ctx, probeT2, probeT3);
    } catch (RpcError e) {
      submitErrorTrailer(callId, e.code(), e.rpcMessage());
    } catch (Exception e) {
      submitErrorTrailer(
          callId, StatusCode.INTERNAL, e.getMessage() == null ? "error" : e.getMessage());
    }
  }

  private void runDispatch(
      AsterCall call,
      MethodDispatcher method,
      Object instance,
      CallContext ctx,
      long probeT2,
      long probeT3) {
    long callId = call.callId();
    try {
      switch (method) {
        case UnaryDispatcher u -> {
          long probeT4 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;
          byte[] responseBytes = u.invoke(instance, call.request(), codec, ctx);
          long probeT5 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;
          byte[] responseFrame = AsterFraming.encodeFrame(responseBytes, (byte) 0);
          byte[] trailerFrame =
              AsterFraming.encodeFrame(okTrailerBytes(), AsterFraming.FLAG_TRAILER);
          submitResponse(callId, new CallResponse(responseFrame, trailerFrame));
          if (site.aster.probe.AsterProbes.ENABLED) {
            long probeT6 = System.nanoTime();
            site.aster.probe.AsterProbes.recordServer(probeT2, probeT3, probeT4, probeT5, probeT6);
          }
        }
        case ServerStreamDispatcher s -> {
          ReactorResponseStream out = new ReactorResponseStream(reactor, callId, foryHeaderCodec);
          try {
            s.invoke(instance, call.request(), codec, ctx, out);
            out.complete();
          } catch (Throwable t) {
            out.fail(t);
          }
        }
        case ClientStreamDispatcher c -> {
          ReactorRequestStream in = new ReactorRequestStream(reactor, callId, call.request());
          try {
            byte[] responseBytes = c.invoke(instance, in, codec, ctx);
            byte[] responseFrame = AsterFraming.encodeFrame(responseBytes, (byte) 0);
            byte[] trailerFrame =
                AsterFraming.encodeFrame(okTrailerBytes(), AsterFraming.FLAG_TRAILER);
            submitResponse(callId, new CallResponse(responseFrame, trailerFrame));
          } catch (RpcError e) {
            submitErrorTrailer(callId, e.code(), e.rpcMessage());
          } catch (Exception e) {
            submitErrorTrailer(
                callId, StatusCode.INTERNAL, e.getMessage() == null ? "error" : e.getMessage());
          }
        }
        case BidiStreamDispatcher b -> {
          ReactorRequestStream in = new ReactorRequestStream(reactor, callId, call.request());
          ReactorResponseStream out = new ReactorResponseStream(reactor, callId, foryHeaderCodec);
          try {
            b.invoke(instance, in, codec, ctx, out);
            out.complete();
          } catch (Throwable t) {
            out.fail(t);
          }
        }
      }
    } catch (RpcError e) {
      submitErrorTrailer(callId, e.code(), e.rpcMessage());
    } catch (Exception e) {
      submitErrorTrailer(
          callId, StatusCode.INTERNAL, e.getMessage() == null ? "error" : e.getMessage());
    }
  }

  /**
   * Resolve the service instance for an inbound call (multiplexed-streams spec §6 / §7.5).
   *
   * <ul>
   *   <li>SHARED service + sessionId == 0 → return the singleton.
   *   <li>SHARED service + sessionId != 0 → scope mismatch (peer expected a session-bound service
   *       at this name).
   *   <li>SESSION service + sessionId == 0 → scope mismatch (peer should have allocated a sessionId
   *       and sent it on the StreamHeader).
   *   <li>SESSION service + sessionId &gt; lastOpenedSessionId → create the session if the cap
   *       allows; bump {@code lastOpenedSessionId} to {@code sessionId}.
   *   <li>SESSION service + sessionId &lt;= lastOpenedSessionId but not in the active map →
   *       graveyard hit, NOT_FOUND.
   *   <li>SESSION service + sessionId already in the active map → return the existing instance.
   * </ul>
   */
  private Object resolveInstance(RegisteredService svc, AsterCall call, StreamHeader header) {
    int sessionId = header.sessionId();
    boolean isSessionScope = svc.descriptor.scope() == Scope.SESSION;

    if (!isSessionScope) {
      if (sessionId != 0) {
        throw new SessionScopeMismatchException(
            "service '"
                + svc.descriptor.name()
                + "' is SHARED but call carried sessionId="
                + sessionId);
      }
      return svc.sharedInstance;
    }

    if (sessionId == 0) {
      throw new SessionScopeMismatchException(
          "service '"
              + svc.descriptor.name()
              + "' is SESSION-scoped; call must carry a non-zero sessionId");
    }

    ConnectionState state =
        connections.computeIfAbsent(
            call.connectionId(), id -> new ConnectionState(maxSessionsPerConnection));
    SessionKey key = new SessionKey(call.connectionId(), sessionId, svc.descriptor.implClass());

    // Single-poll-thread invariant means computeIfAbsent on the connections map is sequential per
    // connection; the synchronized block here guards against the race where multiple service
    // classes on the same sessionId race in (rare but possible if a future change parallelises
    // the poll loop).
    synchronized (state) {
      if (state.activeSessions.contains(key)) {
        return sessionRegistry.getOrCreate(key, call.peerId(), svc.factory);
      }
      if (sessionId <= state.lastOpenedSessionId) {
        throw new SessionNotFoundException(
            "session " + sessionId + " was previously opened on this connection and is now closed");
      }
      // Spec §7.5: cap counts active sessions only — a fresh sessionId > lastOpenedSessionId
      // beyond the cap is rejected with RESOURCE_EXHAUSTED *without* bumping the graveyard
      // counter, so a subsequent retry with the same id surfaces RESOURCE_EXHAUSTED again
      // rather than NOT_FOUND.
      if (state.activeSessions.size() >= state.maxSessions) {
        throw new SessionLimitException(
            "connection has reached max_sessions_per_connection=" + state.maxSessions);
      }
      state.lastOpenedSessionId = sessionId;
      state.activeSessions.add(key);
    }
    return sessionRegistry.getOrCreate(key, call.peerId(), svc.factory);
  }

  // ───── Per-connection state + typed errors ───────────────────────────────

  /**
   * Per-connection session state (spec §7.5). Holds the active session key set, the monotonic
   * graveyard counter, and the per-connection cap. Mutated under {@code synchronized(this)} from
   * the poll thread.
   */
  private static final class ConnectionState {
    final java.util.Set<SessionKey> activeSessions = new java.util.HashSet<>();
    int lastOpenedSessionId;
    final int maxSessions;

    ConnectionState(int maxSessions) {
      this.maxSessions = maxSessions;
    }
  }

  /** Thrown when a peer references a sessionId that has already been closed. */
  private static final class SessionNotFoundException extends RuntimeException {
    SessionNotFoundException(String message) {
      super(message);
    }
  }

  /** Thrown when a peer's open_session would exceed {@code max_sessions_per_connection}. */
  private static final class SessionLimitException extends RuntimeException {
    SessionLimitException(String message) {
      super(message);
    }
  }

  /** Thrown when a SHARED call carries a sessionId or a SESSION call carries sessionId=0. */
  private static final class SessionScopeMismatchException extends RuntimeException {
    SessionScopeMismatchException(String message) {
      super(message);
    }
  }

  private StreamHeader decodeStreamHeader(byte[] bytes) {
    if (bytes.length == 0) {
      return new StreamHeader("", "", 0, 0, (short) 0, (byte) 0, List.of(), List.of(), 0);
    }
    Object decoded = foryHeaderCodec.decode(bytes, StreamHeader.class);
    if (!(decoded instanceof StreamHeader sh)) {
      throw new IrohException("StreamHeader decode returned " + decoded);
    }
    return sh;
  }

  private CallContext buildCallContext(
      StreamHeader header, MethodDispatcher method, AsterCall call) {
    Map<String, String> metadata = new HashMap<>();
    List<String> keys = header.metadataKeys();
    List<String> vals = header.metadataValues();
    int n = Math.min(keys.size(), vals.size());
    for (int i = 0; i < n; i++) {
      metadata.put(keys.get(i), vals.get(i));
    }
    Map<String, String> attributes = new HashMap<>();
    Map<String, String> peerAttrs = peerAttributes.get(call.peerId());
    if (peerAttrs != null) {
      attributes.putAll(peerAttrs);
    }
    return CallContext.builder(header.service(), header.method())
        .peer(call.peerId())
        .metadata(metadata)
        .attributes(attributes)
        .deadlineFromRelativeSecs(header.deadline())
        .streaming(method.descriptor().streaming().name().endsWith("STREAM"))
        .pattern(method.descriptor().streaming().name().toLowerCase())
        .idempotent(method.descriptor().idempotent())
        .build();
  }

  private void submitResponse(long callId, CallResponse response) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment respSeg = MemorySegment.NULL;
      MemorySegment trailerSeg = MemorySegment.NULL;
      if (response.responseFrame().length > 0) {
        respSeg = arena.allocate(ValueLayout.JAVA_BYTE, response.responseFrame().length);
        respSeg.copyFrom(MemorySegment.ofArray(response.responseFrame()));
      }
      if (response.trailerFrame().length > 0) {
        trailerSeg = arena.allocate(ValueLayout.JAVA_BYTE, response.trailerFrame().length);
        trailerSeg.copyFrom(MemorySegment.ofArray(response.trailerFrame()));
      }
      reactor.submit(callId, respSeg, trailerSeg);
    }
  }

  private byte[] okTrailerBytes() {
    byte[] cached = okTrailerBytesCache;
    if (cached == null) {
      cached = foryHeaderCodec.encode(RpcStatus.ok());
      okTrailerBytesCache = cached;
    }
    return cached;
  }

  private void submitErrorTrailer(long callId, StatusCode code, String message) {
    RpcStatus status =
        new RpcStatus(code.value(), message == null ? "" : message, List.of(), List.of());
    byte[] trailerPayload = foryHeaderCodec.encode(status);
    byte[] trailerFrame = AsterFraming.encodeFrame(trailerPayload, AsterFraming.FLAG_TRAILER);
    submitResponse(callId, new CallResponse(new byte[0], trailerFrame));
  }

  // ───── Codec / wire type bootstrap ───────────────────────────────────────

  private ForyCodec registerFrameworkWireTypes(Codec userCodec) {
    // StreamHeader / CallHeader / RpcStatus are always Fory xlang regardless of what the user
    // picked for their payloads. If the user's codec IS a ForyCodec, register them on its Fory
    // so header decode and user decode share one pump. Otherwise, build a dedicated Fory
    // purely for framework wire types.
    ForyCodec headerCodec = userCodec instanceof ForyCodec fc ? fc : new ForyCodec();
    try {
      site.aster.codec.ForyTags.register(
          headerCodec.fory(), StreamHeader.class, "_aster/StreamHeader");
    } catch (Throwable ignored) {
    }
    try {
      site.aster.codec.ForyTags.register(headerCodec.fory(), CallHeader.class, "_aster/CallHeader");
    } catch (Throwable ignored) {
    }
    try {
      site.aster.codec.ForyTags.register(headerCodec.fory(), RpcStatus.class, "_aster/RpcStatus");
    } catch (Throwable ignored) {
    }
    return headerCodec;
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
    private SessionRegistry sessionRegistry = new InMemorySessionRegistry();
    private List<Interceptor> interceptors = List.of();
    private List<String> alpns = List.of(ASTER_ALPN, CONSUMER_ADMISSION_ALPN);
    private int ringCapacity = DEFAULT_RING_CAPACITY;
    private int maxSessionsPerConnection = DEFAULT_MAX_SESSIONS_PER_CONNECTION;

    private Builder() {}

    public Builder config(AsterConfig config) {
      this.config = config;
      return this;
    }

    public Builder codec(Codec codec) {
      this.codec = codec;
      return this;
    }

    public Builder sessionRegistry(SessionRegistry registry) {
      this.sessionRegistry = registry;
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
     * is reached, further session-create requests from that connection fail with
     * RESOURCE_EXHAUSTED. Default {@link #DEFAULT_MAX_SESSIONS_PER_CONNECTION}.
     */
    public Builder maxSessionsPerConnection(int max) {
      if (max < 1) {
        throw new IllegalArgumentException("maxSessionsPerConnection must be >= 1, got " + max);
      }
      this.maxSessionsPerConnection = max;
      return this;
    }

    /**
     * Register a SHARED-scope service instance. The dispatcher for its class must be on the
     * classpath.
     */
    public Builder service(Object instance) {
      ServiceDispatcher d = findDispatcherFor(instance.getClass());
      if (d.descriptor().scope() != Scope.SHARED) {
        throw new IllegalArgumentException(
            "service() requires a SHARED-scope service; use sessionService() for "
                + d.descriptor().scope());
      }
      services.put(d.descriptor().name(), new RegisteredService(d.descriptor(), d, instance, null));
      return this;
    }

    /** Register a SESSION-scope service class with a per-peer factory. */
    public Builder sessionService(Class<?> implClass, Function<String, Object> factory) {
      ServiceDispatcher d = findDispatcherFor(implClass);
      if (d.descriptor().scope() != Scope.SESSION) {
        throw new IllegalArgumentException(
            "sessionService() requires a SESSION-scope service; got " + d.descriptor().scope());
      }
      services.put(d.descriptor().name(), new RegisteredService(d.descriptor(), d, null, factory));
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
      List<byte[]> alpnBytes = alpns.stream().map(s -> s.getBytes(StandardCharsets.UTF_8)).toList();

      CompletableFuture<IrohNode> nodeFuture;
      if (config != null && config.storagePath() != null && !config.storagePath().isBlank()) {
        nodeFuture = IrohNode.persistentWithAlpns(config.storagePath(), alpnBytes);
      } else {
        nodeFuture = IrohNode.memoryWithAlpns(alpnBytes);
      }

      Builder self = this;
      return nodeFuture
          .thenApply(
              node -> {
                try {
                  Reactor reactor =
                      new Reactor(node.runtime().nativeHandle(), node.nodeHandle(), ringCapacity);
                  return new AsterServer(self, node, reactor);
                } catch (Exception e) {
                  node.close();
                  throw new IrohException("Failed to create reactor: " + e.getMessage());
                }
              })
          .thenCompose(
              server -> {
                // Publish each registered service's contract collection into a fresh registry
                // doc. Mirrors Python's AsterServer._publish_contracts so the same consumer-side
                // tooling (aster contract gen-client, registry shell) works against Java
                // producers. Non-fatal: on failure we still return the server so local-only use
                // cases (e.g. tests without docs backing) continue to work.
                List<site.aster.server.spi.ServiceDispatcher> dispatchers = new ArrayList<>();
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
                            // Non-fatal: publication is an extra. Tests that spin up without
                            // docs backing still get a working server.
                            System.err.println(
                                "AsterServer: contract publication failed (non-fatal): " + err);
                            err.printStackTrace(System.err);
                          }
                          return server;
                        });
              });
    }
  }

  /** Internal binding of a dispatcher to its instance (SHARED) or factory (SESSION). */
  private record RegisteredService(
      ServiceDescriptor descriptor,
      ServiceDispatcher dispatcher,
      Object sharedInstance,
      Function<String, Object> factory) {}
}
