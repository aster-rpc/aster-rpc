package site.aster.client;

import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.interceptors.Interceptor;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

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
  private final AsterConfig config;
  private final List<Interceptor> interceptors;
  private final Map<String, IrohConnection> connectionCache = new ConcurrentHashMap<>();

  private AsterClient(Builder b, IrohEndpoint endpoint) {
    this.endpoint = endpoint;
    this.codec = b.codec != null ? b.codec : new ForyCodec();
    this.config = b.config;
    this.interceptors = List.copyOf(b.interceptors);
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
   * <p>NOT YET IMPLEMENTED. The networking + framing pipeline lands in commit G alongside the
   * Kotlin MissionControl sample so the wire format can be validated end-to-end against the Python
   * reference before freezing the client surface.
   */
  public <Req, Resp> CompletableFuture<Resp> call(
      NodeAddr target, String service, String method, Req request, Class<Resp> responseType) {
    return CompletableFuture.failedFuture(
        new UnsupportedOperationException(
            "AsterClient.call is not yet implemented — lands in commit G with the Kotlin"
                + " MissionControl sample. Use AsterClient.connect(...) for raw Iroh access in"
                + " the meantime."));
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
