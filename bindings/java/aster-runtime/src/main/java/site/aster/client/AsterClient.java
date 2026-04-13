package site.aster.client;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.handle.IrohStream;
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
 * <p>Day-0 scope: {@link #connect(NodeAddr)} opens a live connection to a target peer and {@link
 * #close()} tears everything down. The {@link #call(NodeAddr, String, String, Object, Class)}
 * network round-trip lands alongside the Kotlin MissionControl sample in commit G, where
 * cross-language framing can be validated against the Python reference in {@code
 * bindings/python/aster/transport/iroh.py}.
 */
public final class AsterClient implements AutoCloseable {

  private final IrohEndpoint endpoint;
  private final Codec codec;
  private final ForyCodec headerCodec;
  private final AsterConfig config;
  private final List<Interceptor> interceptors;
  private final Map<String, IrohConnection> connectionCache = new ConcurrentHashMap<>();

  private AsterClient(Builder b, IrohEndpoint endpoint) {
    this.endpoint = endpoint;
    this.codec = b.codec != null ? b.codec : new ForyCodec();
    this.headerCodec = registerFrameworkWireTypes(this.codec);
    this.config = b.config;
    this.interceptors = List.copyOf(b.interceptors);
  }

  private static ForyCodec registerFrameworkWireTypes(Codec userCodec) {
    // Symmetric with AsterServer: StreamHeader / CallHeader / RpcStatus are always Fory xlang.
    // If the user chose ForyCodec, register them on its Fory so header and payload share one
    // pump. Otherwise, build a dedicated Fory purely for framework wire types.
    ForyCodec header = userCodec instanceof ForyCodec fc ? fc : new ForyCodec();
    try {
      header.fory().register(StreamHeader.class, "_aster/StreamHeader");
    } catch (Throwable ignored) {
    }
    try {
      header.fory().register(CallHeader.class, "_aster/CallHeader");
    } catch (Throwable ignored) {
    }
    try {
      header.fory().register(RpcStatus.class, "_aster/RpcStatus");
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

  /**
   * Return (or open) a connection to the given peer on the Aster ALPN. Connections are cached by
   * peer id — the first call to a peer opens a new connection, subsequent calls to the same peer
   * reuse it.
   */
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

  /**
   * Make a unary RPC call.
   *
   * <p>Opens a fresh bidirectional stream on the cached connection, writes the {@link StreamHeader}
   * as the first frame ({@code HEADER} flag), writes the serialized request as the second frame,
   * finishes the send side, then reads response frames until a {@code TRAILER} frame is seen.
   * Mirrors {@code bindings/python/aster/transport/iroh.py} streaming shape — Python's unary path
   * fuses these steps into a single FFI call ({@code IrohConnection.unary_call}) but the wire is
   * identical.
   *
   * @param target the destination node address
   * @param service service name (e.g. {@code "MissionControl"})
   * @param method method name on that service
   * @param request the request object — encoded via the user codec
   * @param responseType Java class of the response — passed to the codec on decode (Fory xlang uses
   *     the embedded type tag so this is advisory)
   * @return a future that completes with the decoded response, or fails with an {@link RpcError}
   *     for non-OK trailers
   */
  public <Req, Resp> CompletableFuture<Resp> call(
      NodeAddr target, String service, String method, Req request, Class<Resp> responseType) {
    return connect(target)
        .thenCompose(
            conn ->
                conn.openBiAsync()
                    .thenCompose(stream -> doCall(stream, service, method, request, responseType)));
  }

  /**
   * Make a server-streaming RPC call: one request frame out, N response frames in until a {@code
   * TRAILER} closes the stream. The returned future completes with the full list of responses once
   * the trailer lands — this is the simplest shape that proves the wire. A {@link
   * java.util.concurrent.Flow.Publisher}-based variant that delivers frames incrementally is a
   * later addition.
   */
  public <Req, Resp> CompletableFuture<List<Resp>> callServerStream(
      NodeAddr target, String service, String method, Req request, Class<Resp> responseType) {
    return connect(target)
        .thenCompose(
            conn ->
                conn.openBiAsync()
                    .thenCompose(
                        stream ->
                            doServerStreamCall(stream, service, method, request, responseType)));
  }

  private <Req, Resp> CompletableFuture<Resp> doCall(
      IrohStream stream, String service, String method, Req request, Class<Resp> responseType) {
    CompletableFuture<Resp> result = new CompletableFuture<>();
    try {
      StreamHeader header =
          new StreamHeader(
              service,
              method,
              1,
              0,
              (short) 0,
              StreamHeader.SERIALIZATION_XLANG,
              List.of(),
              List.of());
      byte[] headerBytes = headerCodec.encode(header);
      byte[] requestBytes = codec.encode(request);

      byte[] headerFrame = AsterFraming.encodeFrame(headerBytes, AsterFraming.FLAG_HEADER);
      byte[] requestFrame = AsterFraming.encodeFrame(requestBytes, (byte) 0);

      ClientFrameReader reader = new ClientFrameReader(stream);
      stream
          .sendAsync(headerFrame)
          .thenCompose(v -> stream.sendAsync(requestFrame))
          .thenCompose(v -> stream.finishAsync())
          .thenCompose(v -> readUntilTrailer(reader, responseType))
          .whenComplete(
              (resp, err) -> {
                try {
                  stream.close();
                } catch (Exception ignored) {
                  // Best-effort — stream may already be closed.
                }
                if (err != null) {
                  result.completeExceptionally(unwrap(err));
                } else {
                  result.complete(resp);
                }
              });
    } catch (Throwable t) {
      try {
        stream.close();
      } catch (Exception ignored) {
      }
      result.completeExceptionally(t);
    }
    return result;
  }

  private <Resp> CompletableFuture<Resp> readUntilTrailer(
      ClientFrameReader reader, Class<Resp> responseType) {
    List<byte[]> responsePayloads = new ArrayList<>();
    CompletableFuture<Resp> out = new CompletableFuture<>();
    readNextFrame(reader, responseType, responsePayloads, out);
    return out;
  }

  private <Resp> void readNextFrame(
      ClientFrameReader reader,
      Class<Resp> responseType,
      List<byte[]> responsePayloads,
      CompletableFuture<Resp> out) {
    reader
        .readFrame()
        .whenComplete(
            (frame, err) -> {
              if (err != null) {
                out.completeExceptionally(unwrap(err));
                return;
              }
              byte flags = frame.flags();
              if ((flags & AsterFraming.FLAG_TRAILER) != 0) {
                try {
                  RpcStatus status =
                      frame.payload().length == 0
                          ? RpcStatus.ok()
                          : (RpcStatus) headerCodec.decode(frame.payload(), RpcStatus.class);
                  if (status.code() != RpcStatus.OK) {
                    out.completeExceptionally(
                        new RpcError(
                            StatusCode.fromValue(status.code()),
                            status.message() == null ? "" : status.message()));
                    return;
                  }
                  if (responsePayloads.isEmpty()) {
                    out.completeExceptionally(
                        new RpcError(StatusCode.INTERNAL, "OK trailer with no response frame"));
                    return;
                  }
                  if (responsePayloads.size() > 1) {
                    out.completeExceptionally(
                        new RpcError(
                            StatusCode.INTERNAL,
                            "unary call received " + responsePayloads.size() + " response frames"));
                    return;
                  }
                  @SuppressWarnings("unchecked")
                  Resp decoded = (Resp) codec.decode(responsePayloads.get(0), responseType);
                  out.complete(decoded);
                } catch (Throwable t) {
                  out.completeExceptionally(t);
                }
                return;
              }
              if ((flags & AsterFraming.FLAG_ROW_SCHEMA) != 0) {
                // §5.5.2: skip row schema frames on unary — no row-mode support yet.
                readNextFrame(reader, responseType, responsePayloads, out);
                return;
              }
              responsePayloads.add(frame.payload());
              readNextFrame(reader, responseType, responsePayloads, out);
            });
  }

  private <Req, Resp> CompletableFuture<List<Resp>> doServerStreamCall(
      IrohStream stream, String service, String method, Req request, Class<Resp> responseType) {
    CompletableFuture<List<Resp>> result = new CompletableFuture<>();
    try {
      StreamHeader header =
          new StreamHeader(
              service,
              method,
              1,
              0,
              (short) 0,
              StreamHeader.SERIALIZATION_XLANG,
              List.of(),
              List.of());
      byte[] headerBytes = headerCodec.encode(header);
      byte[] requestBytes = codec.encode(request);

      byte[] headerFrame = AsterFraming.encodeFrame(headerBytes, AsterFraming.FLAG_HEADER);
      byte[] requestFrame = AsterFraming.encodeFrame(requestBytes, (byte) 0);

      ClientFrameReader reader = new ClientFrameReader(stream);
      stream
          .sendAsync(headerFrame)
          .thenCompose(v -> stream.sendAsync(requestFrame))
          .thenCompose(v -> stream.finishAsync())
          .thenCompose(v -> collectUntilTrailer(reader, responseType))
          .whenComplete(
              (payloads, err) -> {
                try {
                  stream.close();
                } catch (Exception ignored) {
                  // Best-effort — stream may already be closed.
                }
                if (err != null) {
                  result.completeExceptionally(unwrap(err));
                } else {
                  result.complete(payloads);
                }
              });
    } catch (Throwable t) {
      try {
        stream.close();
      } catch (Exception ignored) {
      }
      result.completeExceptionally(t);
    }
    return result;
  }

  private <Resp> CompletableFuture<List<Resp>> collectUntilTrailer(
      ClientFrameReader reader, Class<Resp> responseType) {
    List<Resp> collected = new ArrayList<>();
    CompletableFuture<List<Resp>> out = new CompletableFuture<>();
    collectNextFrame(reader, responseType, collected, out);
    return out;
  }

  private <Resp> void collectNextFrame(
      ClientFrameReader reader,
      Class<Resp> responseType,
      List<Resp> collected,
      CompletableFuture<List<Resp>> out) {
    reader
        .readFrame()
        .whenComplete(
            (frame, err) -> {
              if (err != null) {
                out.completeExceptionally(unwrap(err));
                return;
              }
              byte flags = frame.flags();
              if ((flags & AsterFraming.FLAG_TRAILER) != 0) {
                try {
                  RpcStatus status =
                      frame.payload().length == 0
                          ? RpcStatus.ok()
                          : (RpcStatus) headerCodec.decode(frame.payload(), RpcStatus.class);
                  if (status.code() != RpcStatus.OK) {
                    out.completeExceptionally(
                        new RpcError(
                            StatusCode.fromValue(status.code()),
                            status.message() == null ? "" : status.message()));
                    return;
                  }
                  out.complete(collected);
                } catch (Throwable t) {
                  out.completeExceptionally(t);
                }
                return;
              }
              if ((flags & AsterFraming.FLAG_ROW_SCHEMA) != 0) {
                collectNextFrame(reader, responseType, collected, out);
                return;
              }
              try {
                @SuppressWarnings("unchecked")
                Resp decoded = (Resp) codec.decode(frame.payload(), responseType);
                collected.add(decoded);
              } catch (Throwable t) {
                out.completeExceptionally(t);
                return;
              }
              collectNextFrame(reader, responseType, collected, out);
            });
  }

  private static Throwable unwrap(Throwable t) {
    return t instanceof java.util.concurrent.CompletionException && t.getCause() != null
        ? t.getCause()
        : t;
  }

  @Override
  public void close() {
    for (IrohConnection conn : connectionCache.values()) {
      try {
        conn.close();
      } catch (Exception ignored) {
        // Best-effort
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
