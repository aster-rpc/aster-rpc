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
 * <p>Unary dispatch is fully operational. Streaming methods are discovered and their method
 * descriptors are published but invocation currently returns an {@link StatusCode#UNIMPLEMENTED}
 * trailer — the Kotlin / Flow bridging needed for real streaming lands in commit G alongside the
 * MissionControl sample.
 *
 * <p>Manifest submission (via {@code RegistryAsync.publishAsync}) is not yet hooked; the manifest
 * is built at startup and exposed via {@link #manifest()} but not actually published. Hook dispatch
 * ({@code IROH_EVENT_HOOK_*} → user callbacks) is likewise deferred.
 */
public final class AsterServer implements AutoCloseable {

  public static final String ASTER_ALPN = "aster/1";
  private static final int DEFAULT_RING_CAPACITY = 256;
  private static final int DEFAULT_POLL_BATCH = 32;
  private static final int POLL_TIMEOUT_MS = 100;

  private final IrohNode node;
  private final Reactor reactor;
  private final Codec codec;
  private final ForyCodec foryHeaderCodec;
  private volatile byte[] okTrailerBytesCache;
  private final Map<String, RegisteredService> services;
  private final SessionRegistry sessionRegistry;
  private final List<Interceptor> interceptors;
  private final AsterConfig config;
  private final List<ServiceDescriptor> manifest;
  private final AtomicBoolean running = new AtomicBoolean(true);
  private final Thread pollThread;
  private final ExecutorService callExecutor;
  private final ExecutorService streamingExecutor;

  private AsterServer(Builder b, IrohNode node, Reactor reactor) {
    this.node = node;
    this.reactor = reactor;
    this.codec = b.codec != null ? b.codec : new ForyCodec();
    this.foryHeaderCodec = registerFrameworkWireTypes(this.codec);
    this.services = Map.copyOf(b.services);
    this.sessionRegistry = b.sessionRegistry;
    this.interceptors = List.copyOf(b.interceptors);
    this.config = b.config;
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
  public List<ServiceDescriptor> manifest() {
    return manifest;
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
    sessionRegistry.clear();
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

  private AsterCall extractCall(MemorySegment slot) {
    long callId = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_CALL_ID);
    long streamId = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_STREAM_ID);
    MemorySegment headerPtr = slot.get(ValueLayout.ADDRESS, Reactor.OFFSET_HEADER_PTR);
    int headerLen = slot.get(ValueLayout.JAVA_INT, Reactor.OFFSET_HEADER_LEN);
    byte headerFlags = slot.get(ValueLayout.JAVA_BYTE, Reactor.OFFSET_HEADER_FLAGS);
    MemorySegment requestPtr = slot.get(ValueLayout.ADDRESS, Reactor.OFFSET_REQUEST_PTR);
    int requestLen = slot.get(ValueLayout.JAVA_INT, Reactor.OFFSET_REQUEST_LEN);
    byte requestFlags = slot.get(ValueLayout.JAVA_BYTE, Reactor.OFFSET_REQUEST_FLAGS);
    MemorySegment peerPtr = slot.get(ValueLayout.ADDRESS, Reactor.OFFSET_PEER_PTR);
    int peerLen = slot.get(ValueLayout.JAVA_INT, Reactor.OFFSET_PEER_LEN);
    byte isSession = slot.get(ValueLayout.JAVA_BYTE, Reactor.OFFSET_IS_SESSION_CALL);

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
        callId, streamId, header, headerFlags, request, requestFlags, peerId, isSession != 0);
  }

  // ───── Dispatch ──────────────────────────────────────────────────────────

  private void dispatchCall(AsterCall call) {
    long callId = call.callId();
    try {
      StreamHeader header = decodeStreamHeader(call.header());
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
      Object instance = resolveInstance(svc, call);
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
        streamingExecutor.execute(() -> runDispatch(call, pinnedMethod, pinnedInstance, pinnedCtx));
        return;
      }

      runDispatch(call, method, instance, ctx);
    } catch (RpcError e) {
      submitErrorTrailer(callId, e.code(), e.rpcMessage());
    } catch (Exception e) {
      submitErrorTrailer(
          callId, StatusCode.INTERNAL, e.getMessage() == null ? "error" : e.getMessage());
    }
  }

  private void runDispatch(
      AsterCall call, MethodDispatcher method, Object instance, CallContext ctx) {
    long callId = call.callId();
    try {
      switch (method) {
        case UnaryDispatcher u -> {
          byte[] responseBytes = u.invoke(instance, call.request(), codec, ctx);
          byte[] responseFrame = AsterFraming.encodeFrame(responseBytes, (byte) 0);
          byte[] trailerFrame =
              AsterFraming.encodeFrame(okTrailerBytes(), AsterFraming.FLAG_TRAILER);
          submitResponse(callId, new CallResponse(responseFrame, trailerFrame));
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

  private Object resolveInstance(RegisteredService svc, AsterCall call) {
    if (svc.descriptor.scope() == Scope.SHARED) {
      return svc.sharedInstance;
    }
    SessionKey key = new SessionKey(call.peerId(), call.streamId(), svc.descriptor.implClass());
    return sessionRegistry.getOrCreate(key, svc.factory);
  }

  private StreamHeader decodeStreamHeader(byte[] bytes) {
    if (bytes.length == 0) {
      return new StreamHeader("", "", 0, 0, (short) 0, (byte) 0, List.of(), List.of());
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
    return CallContext.builder(header.service(), header.method())
        .peer(call.peerId())
        .metadata(metadata)
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
      headerCodec.fory().register(StreamHeader.class, "_aster/StreamHeader");
    } catch (Throwable ignored) {
    }
    try {
      headerCodec.fory().register(CallHeader.class, "_aster/CallHeader");
    } catch (Throwable ignored) {
    }
    try {
      headerCodec.fory().register(RpcStatus.class, "_aster/RpcStatus");
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
    private List<String> alpns = List.of(ASTER_ALPN);
    private int ringCapacity = DEFAULT_RING_CAPACITY;

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
      return nodeFuture.thenApply(
          node -> {
            try {
              Reactor reactor =
                  new Reactor(node.runtime().nativeHandle(), node.nodeHandle(), ringCapacity);
              return new AsterServer(self, node, reactor);
            } catch (Exception e) {
              node.close();
              throw new IrohException("Failed to create reactor: " + e.getMessage());
            }
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
