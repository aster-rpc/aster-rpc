package site.aster.client;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.Executor;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.ffi.AsterCall;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.interceptors.Interceptor;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;
import site.aster.server.AsterFraming;
import site.aster.server.AsterServer;
import site.aster.server.wire.CallHeader;
import site.aster.server.wire.RpcStatus;
import site.aster.server.wire.StreamHeader;

/**
 * Client-side entry point for calling Aster RPC services. Manages one {@link IrohEndpoint}, a
 * connection cache keyed by peer id, a {@link Codec} for request/response serialization, and an
 * interceptor chain.
 *
 * <p>All outbound calls flow through the multiplexed-streams primitive ({@code aster_call_*}): each
 * call acquires a handle from the connection's per-connection pool (spec §8), sends framed request
 * bytes, drains the response frames through the trailer, and releases the handle so the underlying
 * stream returns to the pool. The framing state machine lives in {@code core}; this class is a thin
 * shim.
 */
public final class AsterClient implements AutoCloseable {

  private final IrohEndpoint endpoint;
  private final Codec codec;
  private final ForyCodec headerCodec;
  private final AsterConfig config;
  private final List<Interceptor> interceptors;
  private final Map<String, IrohConnection> connectionCache = new ConcurrentHashMap<>();
  private final Executor callExecutor = CallExecutor.INSTANCE;

  private AsterClient(Builder b, IrohEndpoint endpoint) {
    this.endpoint = endpoint;
    this.codec = b.codec != null ? b.codec : new ForyCodec();
    this.headerCodec = registerFrameworkWireTypes(this.codec);
    this.config = b.config;
    this.interceptors = List.copyOf(b.interceptors);
  }

  private static ForyCodec registerFrameworkWireTypes(Codec userCodec) {
    ForyCodec header = userCodec instanceof ForyCodec fc ? fc : new ForyCodec();
    try {
      site.aster.codec.ForyTags.register(header.fory(), StreamHeader.class, "_aster/StreamHeader");
    } catch (Throwable ignored) {
    }
    try {
      site.aster.codec.ForyTags.register(header.fory(), CallHeader.class, "_aster/CallHeader");
    } catch (Throwable ignored) {
    }
    try {
      site.aster.codec.ForyTags.register(header.fory(), RpcStatus.class, "_aster/RpcStatus");
    } catch (Throwable ignored) {
    }
    return header;
  }

  /** The underlying endpoint's node id (hex). */
  public String nodeId() {
    return endpoint.nodeId();
  }

  public IrohEndpoint endpoint() {
    return endpoint;
  }

  public Codec codec() {
    return codec;
  }

  public AsterConfig config() {
    return config;
  }

  public List<Interceptor> interceptors() {
    return interceptors;
  }

  public CompletableFuture<IrohConnection> connect(NodeAddr target) {
    String peerId = target.endpointId();
    IrohConnection existing = connectionCache.get(peerId);
    if (existing != null) {
      return CompletableFuture.completedFuture(existing);
    }
    return endpoint
        .connectNodeAddrAsync(target, AsterServer.ASTER_ALPN)
        .thenApply(
            conn -> {
              IrohConnection prior = connectionCache.putIfAbsent(peerId, conn);
              if (prior != null) {
                conn.close();
                return prior;
              }
              return conn;
            });
  }

  /** Make a unary RPC call against a SHARED-scoped service (sessionId=0). */
  public <Req, Resp> CompletableFuture<Resp> call(
      NodeAddr target, String service, String method, Req request, Class<Resp> responseType) {
    return connect(target)
        .thenComposeAsync(
            conn -> runUnary(conn, 0, service, method, request, responseType), callExecutor);
  }

  /**
   * Open a session for the given peer (multiplexed-streams spec §6 / §7.5). Allocates a fresh
   * monotonic {@code sessionId} on the underlying connection; subsequent calls through the returned
   * {@link ClientSession} carry that {@code sessionId} on every {@code StreamHeader} so the server
   * can route them to the same per-peer SESSION-scoped service instance.
   *
   * <p>There is no "open session" RPC — sessions are created implicitly server-side on first
   * arrival of a stream with a fresh {@code sessionId}. This call only allocates the id locally.
   * Returned sessions need not be explicitly closed: server-side reap happens when the connection
   * drops.
   */
  public CompletableFuture<ClientSession> openSession(NodeAddr target) {
    return connect(target).thenApply(conn -> new ClientSession(this, conn, conn.nextSessionId()));
  }

  /**
   * Make a server-streaming RPC call: one request frame out, N response frames in until a {@code
   * TRAILER} closes the stream. The returned future completes with the full list of responses once
   * the trailer lands.
   */
  public <Req, Resp> CompletableFuture<List<Resp>> callServerStream(
      NodeAddr target, String service, String method, Req request, Class<Resp> responseType) {
    return connect(target)
        .thenComposeAsync(
            conn -> runServerStream(conn, 0, service, method, request, responseType), callExecutor);
  }

  /**
   * Make a client-streaming RPC call: N request frames out (last marked with {@link
   * AsterFraming#FLAG_END_STREAM}), one response frame in, then a {@code TRAILER}. Buffered shape.
   */
  public <Req, Resp> CompletableFuture<Resp> callClientStream(
      NodeAddr target,
      String service,
      String method,
      Iterable<Req> requests,
      Class<Resp> responseType) {
    List<Req> materialized = materialize(requests);
    if (materialized.isEmpty()) {
      return CompletableFuture.failedFuture(
          new IllegalArgumentException(
              "callClientStream requires at least one request frame; the wire format delivers"
                  + " the first frame inline with the call to bootstrap the dispatcher"));
    }
    return connect(target)
        .thenComposeAsync(
            conn -> runClientStream(conn, 0, service, method, materialized, responseType),
            callExecutor);
  }

  /**
   * Make a buffered bidirectional-streaming RPC call: all requests are sent before any response is
   * read. Use {@link #openBidiStream} for true interleaving.
   */
  public <Req, Resp> CompletableFuture<List<Resp>> callBidiStream(
      NodeAddr target,
      String service,
      String method,
      Iterable<Req> requests,
      Class<Resp> responseType) {
    List<Req> materialized = materialize(requests);
    if (materialized.isEmpty()) {
      return CompletableFuture.failedFuture(
          new IllegalArgumentException(
              "callBidiStream requires at least one request frame; the wire format delivers"
                  + " the first frame inline with the call to bootstrap the dispatcher"));
    }
    return connect(target)
        .thenComposeAsync(
            conn -> runBidiBuffered(conn, 0, service, method, materialized, responseType),
            callExecutor);
  }

  /**
   * Open a true interleaved bidi-streaming call. Returns a {@link BidiCall} object the caller
   * drives directly via {@link BidiCall#send}, {@link BidiCall#recv}, {@link BidiCall#complete}.
   */
  public <Req, Resp> CompletableFuture<BidiCall<Req, Resp>> openBidiStream(
      NodeAddr target, String service, String method, Class<Resp> responseType) {
    return connect(target)
        .thenApplyAsync(
            conn -> openBidiStreamOn(conn, 0, service, method, responseType), callExecutor);
  }

  <Req, Resp> BidiCall<Req, Resp> openBidiStreamOn(
      IrohConnection conn, int sessionId, String service, String method, Class<Resp> responseType) {
    AsterCall call = acquireStreamingOn(conn);
    try {
      sendStreamHeader(call, sessionId, service, method);
    } catch (Throwable t) {
      call.discard();
      throw reThrow(t);
    }
    return new BidiCall<Req, Resp>(call, codec, headerCodec, responseType);
  }

  <Req, Resp> CompletableFuture<Resp> runUnary(
      IrohConnection conn,
      int sessionId,
      String service,
      String method,
      Req request,
      Class<Resp> responseType) {
    CompletableFuture<Resp> out = new CompletableFuture<>();
    callExecutor.execute(
        () -> {
          try {
            byte[] headerBytes = headerCodec.encode(buildStreamHeader(sessionId, service, method));
            byte[] headerFrame = AsterFraming.encodeFrame(headerBytes, AsterFraming.FLAG_HEADER);
            byte[] requestBytes = codec.encode(request);
            byte[] requestFrame =
                AsterFraming.encodeFrame(requestBytes, AsterFraming.FLAG_END_STREAM);
            byte[] requestPair = new byte[headerFrame.length + requestFrame.length];
            System.arraycopy(headerFrame, 0, requestPair, 0, headerFrame.length);
            System.arraycopy(requestFrame, 0, requestPair, headerFrame.length, requestFrame.length);

            long probeT0 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;
            AsterCall.UnaryResult result =
                AsterCall.unary(
                    conn.runtime().nativeHandle(), conn.nativeHandle(), sessionId, requestPair);
            if (site.aster.probe.AsterProbes.ENABLED) {
              site.aster.probe.AsterProbes.recordClient(probeT0, System.nanoTime());
            }

            // Decode trailer first; non-OK status surfaces as RpcError.
            RpcStatus status =
                result.trailerPayload().length == 0
                    ? RpcStatus.ok()
                    : (RpcStatus) headerCodec.decode(result.trailerPayload(), RpcStatus.class);
            if (status.code() != RpcStatus.OK) {
              throw new RpcError(
                  StatusCode.fromValue(status.code()),
                  status.message() == null ? "" : status.message());
            }
            if (result.responsePayload() == null) {
              throw new RpcError(StatusCode.INTERNAL, "OK trailer with no response frame");
            }
            @SuppressWarnings("unchecked")
            Resp decoded = (Resp) codec.decode(result.responsePayload(), responseType);
            out.complete(decoded);
          } catch (Throwable t) {
            out.completeExceptionally(unwrap(t));
          }
        });
    return out;
  }

  <Req, Resp> CompletableFuture<List<Resp>> runServerStream(
      IrohConnection conn,
      int sessionId,
      String service,
      String method,
      Req request,
      Class<Resp> responseType) {
    CompletableFuture<List<Resp>> out = new CompletableFuture<>();
    callExecutor.execute(
        () -> {
          AsterCall call = null;
          try {
            call = acquireStreamingOn(conn);
            sendStreamHeader(call, sessionId, service, method);
            byte[] requestBytes = codec.encode(request);
            call.sendFrame(AsterFraming.encodeFrame(requestBytes, AsterFraming.FLAG_END_STREAM));
            List<Resp> collected = drainStreaming(call, responseType);
            call.release();
            call = null;
            out.complete(collected);
          } catch (Throwable t) {
            if (call != null) call.discard();
            out.completeExceptionally(unwrap(t));
          }
        });
    return out;
  }

  <Req, Resp> CompletableFuture<Resp> runClientStream(
      IrohConnection conn,
      int sessionId,
      String service,
      String method,
      List<Req> requests,
      Class<Resp> responseType) {
    CompletableFuture<Resp> out = new CompletableFuture<>();
    callExecutor.execute(
        () -> {
          AsterCall call = null;
          try {
            call = acquireStreamingOn(conn);
            sendStreamHeader(call, sessionId, service, method);
            for (int i = 0; i < requests.size(); i++) {
              byte[] reqBytes = codec.encode(requests.get(i));
              byte flags = (i == requests.size() - 1) ? AsterFraming.FLAG_END_STREAM : 0;
              call.sendFrame(AsterFraming.encodeFrame(reqBytes, flags));
            }
            UnaryResult<Resp> result = drainUnary(call, responseType);
            call.release();
            call = null;
            out.complete(result.value());
          } catch (Throwable t) {
            if (call != null) call.discard();
            out.completeExceptionally(unwrap(t));
          }
        });
    return out;
  }

  <Req, Resp> CompletableFuture<List<Resp>> runBidiBuffered(
      IrohConnection conn,
      int sessionId,
      String service,
      String method,
      List<Req> requests,
      Class<Resp> responseType) {
    CompletableFuture<List<Resp>> out = new CompletableFuture<>();
    callExecutor.execute(
        () -> {
          AsterCall call = null;
          try {
            call = acquireStreamingOn(conn);
            sendStreamHeader(call, sessionId, service, method);
            for (int i = 0; i < requests.size(); i++) {
              byte[] reqBytes = codec.encode(requests.get(i));
              byte flags = (i == requests.size() - 1) ? AsterFraming.FLAG_END_STREAM : 0;
              call.sendFrame(AsterFraming.encodeFrame(reqBytes, flags));
            }
            List<Resp> collected = drainStreaming(call, responseType);
            call.release();
            call = null;
            out.complete(collected);
          } catch (Throwable t) {
            if (call != null) call.discard();
            out.completeExceptionally(unwrap(t));
          }
        });
    return out;
  }

  // -------------------------------------------------------------------------
  // Shared helpers
  // -------------------------------------------------------------------------

  private AsterCall acquireOn(IrohConnection conn, int sessionId) {
    // Spec §6: sessionId == 0 selects the SHARED pool for stateless calls; non-zero acquires
    // from (or lazily creates) the per-session pool keyed on this id.
    return AsterCall.acquire(conn.runtime().nativeHandle(), conn.nativeHandle(), sessionId);
  }

  /**
   * Acquire a call handle for a <strong>streaming</strong> RPC pattern (server-stream,
   * client-stream, bidi). Bypasses the per-connection multiplexed-stream pool entirely and opens a
   * dedicated substream — per {@code ffi_spec/Aster-multiplexed-streams.md} §3 line 65, "streaming
   * substreams don't count against any pool." Without this bypass, a streaming call would hold a
   * pool slot and block concurrent unary calls on the same session (spec §4.4). The session id
   * still ships in the {@code StreamHeader}; only pool accounting is bypassed.
   */
  private AsterCall acquireStreamingOn(IrohConnection conn) {
    return AsterCall.acquireStreaming(conn.runtime().nativeHandle(), conn.nativeHandle());
  }

  private void sendStreamHeader(AsterCall call, int sessionId, String service, String method) {
    byte[] headerBytes = headerCodec.encode(buildStreamHeader(sessionId, service, method));
    call.sendFrame(AsterFraming.encodeFrame(headerBytes, AsterFraming.FLAG_HEADER));
  }

  private static StreamHeader buildStreamHeader(int sessionId, String service, String method) {
    return new StreamHeader(
        service,
        method,
        1,
        0,
        (short) 0,
        StreamHeader.SERIALIZATION_XLANG,
        List.of(),
        List.of(),
        sessionId);
  }

  /** Record-style holder for drainUnary — lets us distinguish "empty OK" from "no frame". */
  private record UnaryResult<T>(T value) {}

  private <Resp> UnaryResult<Resp> drainUnary(AsterCall call, Class<Resp> responseType) {
    byte[] responsePayload = null;
    int frameCount = 0;
    while (true) {
      AsterCall.RecvFrame frame = call.recvFrame(0);
      if (frame instanceof AsterCall.RecvFrame.EndOfStream) {
        throw new RpcError(StatusCode.INTERNAL, "stream ended before trailer");
      }
      if (frame instanceof AsterCall.RecvFrame.Timeout) {
        // timeout_ms=0 means block indefinitely in the FFI contract, so this
        // branch is unreachable in practice; treat as a transport error.
        throw new RpcError(StatusCode.INTERNAL, "unexpected recv timeout");
      }
      AsterCall.RecvFrame.Ok ok = (AsterCall.RecvFrame.Ok) frame;
      byte flags = ok.flags();
      if ((flags & AsterFraming.FLAG_TRAILER) != 0) {
        RpcStatus status =
            ok.payload().length == 0
                ? RpcStatus.ok()
                : (RpcStatus) headerCodec.decode(ok.payload(), RpcStatus.class);
        if (status.code() != RpcStatus.OK) {
          throw new RpcError(
              StatusCode.fromValue(status.code()),
              status.message() == null ? "" : status.message());
        }
        if (responsePayload == null) {
          throw new RpcError(StatusCode.INTERNAL, "OK trailer with no response frame");
        }
        if (frameCount > 1) {
          throw new RpcError(
              StatusCode.INTERNAL, "unary call received " + frameCount + " response frames");
        }
        @SuppressWarnings("unchecked")
        Resp decoded = (Resp) codec.decode(responsePayload, responseType);
        return new UnaryResult<>(decoded);
      }
      if ((flags & AsterFraming.FLAG_ROW_SCHEMA) != 0) {
        // skip row-schema frames (no row-mode support yet)
        continue;
      }
      responsePayload = ok.payload();
      frameCount++;
    }
  }

  private <Resp> List<Resp> drainStreaming(AsterCall call, Class<Resp> responseType) {
    List<Resp> collected = new ArrayList<>();
    while (true) {
      AsterCall.RecvFrame frame = call.recvFrame(0);
      if (frame instanceof AsterCall.RecvFrame.EndOfStream) {
        throw new RpcError(StatusCode.INTERNAL, "stream ended before trailer");
      }
      if (frame instanceof AsterCall.RecvFrame.Timeout) {
        throw new RpcError(StatusCode.INTERNAL, "unexpected recv timeout");
      }
      AsterCall.RecvFrame.Ok ok = (AsterCall.RecvFrame.Ok) frame;
      byte flags = ok.flags();
      if ((flags & AsterFraming.FLAG_TRAILER) != 0) {
        RpcStatus status =
            ok.payload().length == 0
                ? RpcStatus.ok()
                : (RpcStatus) headerCodec.decode(ok.payload(), RpcStatus.class);
        if (status.code() != RpcStatus.OK) {
          throw new RpcError(
              StatusCode.fromValue(status.code()),
              status.message() == null ? "" : status.message());
        }
        return collected;
      }
      if ((flags & AsterFraming.FLAG_ROW_SCHEMA) != 0) {
        continue;
      }
      @SuppressWarnings("unchecked")
      Resp decoded = (Resp) codec.decode(ok.payload(), responseType);
      collected.add(decoded);
    }
  }

  private static <T> List<T> materialize(Iterable<T> items) {
    List<T> list = new ArrayList<>();
    items.forEach(list::add);
    return list;
  }

  private static Throwable unwrap(Throwable t) {
    return t instanceof java.util.concurrent.CompletionException && t.getCause() != null
        ? t.getCause()
        : t;
  }

  private static RuntimeException reThrow(Throwable t) {
    if (t instanceof RuntimeException re) return re;
    if (t instanceof Error err) throw err;
    return new RuntimeException(t);
  }

  @Override
  public void close() {
    for (IrohConnection conn : connectionCache.values()) {
      try {
        conn.close();
      } catch (Exception ignored) {
      }
    }
    connectionCache.clear();
    endpoint.close();
  }

  public static Builder builder() {
    return new Builder();
  }

  public static final class Builder {
    private AsterConfig config;
    private Codec codec;
    private List<Interceptor> interceptors = List.of();

    private Builder() {}

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

    public CompletableFuture<AsterClient> build() {
      site.aster.handle.IrohRuntime runtime = site.aster.handle.IrohRuntime.create();
      site.aster.config.EndpointConfig endpointConfig =
          new site.aster.config.EndpointConfig().alpns(List.of(AsterServer.ASTER_ALPN));
      Builder self = this;
      return runtime.endpointCreateAsync(endpointConfig).thenApply(ep -> new AsterClient(self, ep));
    }
  }
}
