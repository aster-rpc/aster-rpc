package site.aster.client;

import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.Executor;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.config.AsterIdentity;
import site.aster.ffi.AsterEnvelope;
import site.aster.ffi.Rpc;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.interceptors.Interceptor;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;
import site.aster.registry.ServiceSummary;
import site.aster.server.AsterServer;
import site.aster.server.wire.RpcStatus;
import site.aster.trust.ConsumerAdmissionClient;
import site.aster.trust.ConsumerAdmissionHandler;
import site.aster.trust.ConsumerAdmissionWire;

/**
 * Client-side entry point for calling Aster RPC services. Manages one {@link IrohEndpoint}, a
 * connection cache keyed by peer id, a {@link Codec} for request/response serialization, and an
 * interceptor chain.
 *
 * <p>All outbound calls go through Rust's pooled {@code RpcConnection} (C ABI 3.5, {@link Rpc}):
 * unary calls in one native round trip on the connection's pooled streams, streaming calls on
 * dedicated substreams. Rust builds the header, frames, compresses and decodes trailers; this class
 * encodes payloads and maps statuses to {@link RpcError}.
 */
public final class AsterClient implements AutoCloseable {

  private final IrohEndpoint endpoint;
  private final Codec codec;
  private final AsterConfig config;
  private final List<Interceptor> interceptors;
  private final Map<String, IrohConnection> connectionCache = new ConcurrentHashMap<>();
  private final Executor callExecutor = CallExecutor.INSTANCE;

  /**
   * Inner credential JSON to present during consumer admission. Empty string ⇒ open-gate / dev
   * mode; non-empty ⇒ the consumer-role {@code [[peers]]} entry from an {@code .aster-identity}
   * file, serialized via {@link AsterIdentity#credentialJson}.
   */
  private final String admissionCredentialJson;

  private final String admissionIidToken;

  /**
   * After successful admission against a given {@link NodeAddr}, the RPC endpoint address for that
   * peer (taken from any returned service's {@code channels["rpc"]}). Keyed by the caller-supplied
   * admission target's endpoint id; value is the decoded RPC-channel {@link NodeAddr}. Misses here
   * fall through to using the caller-supplied address directly.
   */
  private final Map<String, NodeAddr> admittedRpcAddr = new ConcurrentHashMap<>();

  /** False: dial {@code aster/1} directly, without the consumer-admission handshake. */
  private final boolean consumerAdmission;

  private AsterClient(Builder b, IrohEndpoint endpoint) {
    this.endpoint = endpoint;
    this.codec = b.codec != null ? b.codec : new ForyCodec();
    this.config = b.config;
    this.interceptors = List.copyOf(b.interceptors);
    this.admissionCredentialJson =
        b.admissionCredentialJson == null ? "" : b.admissionCredentialJson;
    this.admissionIidToken = b.admissionIidToken == null ? "" : b.admissionIidToken;
    this.consumerAdmission = b.consumerAdmission;
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
   * Connect to {@code target} on the RPC ALPN, performing the {@code aster.consumer_admission}
   * handshake on the first visit so the producer registers the peer and hands back the RPC-channel
   * address to dial. Subsequent calls reuse both the admission outcome (cached by target's endpoint
   * id) and the underlying QUIC connection (cached by RPC peer id).
   */
  public CompletableFuture<IrohConnection> connect(NodeAddr target) {
    return admit(target)
        .thenCompose(
            rpcAddr -> {
              String rpcPeerId = rpcAddr.endpointId();
              IrohConnection existing = connectionCache.get(rpcPeerId);
              if (existing != null) {
                return CompletableFuture.completedFuture(existing);
              }
              return endpoint
                  .connectNodeAddrAsync(rpcAddr, AsterServer.ASTER_ALPN)
                  .thenApply(
                      conn -> {
                        IrohConnection prior = connectionCache.putIfAbsent(rpcPeerId, conn);
                        if (prior != null) {
                          conn.close();
                          return prior;
                        }
                        return conn;
                      });
            });
  }

  /**
   * Present the configured credential (or empty in dev mode) to {@code target} over the {@code
   * aster.consumer_admission} ALPN. Memoises the returned RPC-channel address so repeated calls to
   * the same target skip the handshake. Denied admissions surface as {@link RpcError} with code
   * {@link StatusCode#PERMISSION_DENIED}.
   */
  CompletableFuture<NodeAddr> admit(NodeAddr target) {
    if (!consumerAdmission) {
      return CompletableFuture.completedFuture(target);
    }
    String targetKey = target.endpointId();
    NodeAddr cached = admittedRpcAddr.get(targetKey);
    if (cached != null) {
      return CompletableFuture.completedFuture(cached);
    }
    return ConsumerAdmissionClient.performAdmission(
            endpoint, target, admissionCredentialJson, admissionIidToken)
        .thenApply(
            resp -> {
              if (!resp.admitted) {
                throw new RpcError(
                    StatusCode.PERMISSION_DENIED,
                    "consumer admission denied by " + target.endpointId());
              }
              NodeAddr rpcAddr = rpcAddrFrom(resp, target);
              admittedRpcAddr.putIfAbsent(targetKey, rpcAddr);
              return rpcAddr;
            });
  }

  private static NodeAddr rpcAddrFrom(ConsumerAdmissionWire.Response resp, NodeAddr fallback) {
    if (resp.services == null) return fallback;
    for (ServiceSummary s : resp.services) {
      if (s.channels == null) continue;
      String b64 = s.channels.get("rpc");
      if (b64 == null || b64.isEmpty()) continue;
      try {
        return NodeAddr.fromBase64(b64);
      } catch (RuntimeException ignored) {
        // Malformed channel entry — fall through to the next service or the fallback.
      }
    }
    return fallback;
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
   * Open an incremental server-streaming call. Unlike {@link #callServerStream} which buffers the
   * whole response list until the trailer arrives, this variant returns a {@link ServerStreamCall}
   * the caller drives via {@code recv()}/{@code close()}. Required for open-ended streams (e.g. log
   * tails) that never terminate on their own — closing the call tears down the QUIC stream, which
   * is the portable way to stop the server generator.
   */
  public <Req, Resp> CompletableFuture<ServerStreamCall<Resp>> openServerStream(
      NodeAddr target, String service, String method, Req request, Class<Resp> responseType) {
    return connect(target)
        .thenApplyAsync(
            conn -> openServerStreamOn(conn, 0, service, method, request, responseType),
            callExecutor);
  }

  <Req, Resp> ServerStreamCall<Resp> openServerStreamOn(
      IrohConnection conn,
      int sessionId,
      String service,
      String method,
      Req request,
      Class<Resp> responseType) {
    long rt = conn.runtime().nativeHandle();
    long call =
        Rpc.open(
            rt,
            conn.nativeHandle(),
            Rpc.PATTERN_SERVER_STREAM,
            Rpc.Options.of(service, method, sessionId));
    try {
      Rpc.send(rt, call, codec.encode(request), true);
    } catch (Throwable t) {
      Rpc.release(rt, call);
      throw reThrow(t);
    }
    return new ServerStreamCall<>(rt, call, codec, responseType);
  }

  /**
   * Make a client-streaming RPC call: N requests out (the last ends the request stream), one
   * response in, then the trailer. Buffered shape.
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
    long rt = conn.runtime().nativeHandle();
    long call =
        Rpc.open(
            rt,
            conn.nativeHandle(),
            Rpc.PATTERN_BIDI_STREAM,
            Rpc.Options.of(service, method, sessionId));
    return new BidiCall<Req, Resp>(rt, call, codec, responseType);
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
            byte[] requestBytes = codec.encode(request);
            long probeT0 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;
            Rpc.UnaryResult result =
                Rpc.unary(
                    conn.runtime().nativeHandle(),
                    conn.nativeHandle(),
                    Rpc.Options.of(service, method, sessionId),
                    requestBytes);
            if (site.aster.probe.AsterProbes.ENABLED) {
              site.aster.probe.AsterProbes.recordClient(probeT0, System.nanoTime());
            }
            if (result.code() != RpcStatus.OK) {
              throw rpcError(result.status());
            }
            if (result.payload().length == 0) {
              throw new RpcError(StatusCode.INTERNAL, "OK trailer with no response frame");
            }
            @SuppressWarnings("unchecked")
            Resp decoded = (Resp) codec.decode(result.payload(), responseType);
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
          long rt = conn.runtime().nativeHandle();
          long call = 0;
          try {
            call =
                Rpc.open(
                    rt,
                    conn.nativeHandle(),
                    Rpc.PATTERN_SERVER_STREAM,
                    Rpc.Options.of(service, method, sessionId));
            Rpc.send(rt, call, codec.encode(request), true);
            out.complete(drain(rt, call, responseType));
          } catch (Throwable t) {
            out.completeExceptionally(unwrap(t));
          } finally {
            if (call != 0) Rpc.release(rt, call);
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
          long rt = conn.runtime().nativeHandle();
          long call = 0;
          try {
            call =
                Rpc.open(
                    rt,
                    conn.nativeHandle(),
                    Rpc.PATTERN_CLIENT_STREAM,
                    Rpc.Options.of(service, method, sessionId));
            sendAll(rt, call, requests);
            List<Resp> responses = drain(rt, call, responseType);
            if (responses.size() != 1) {
              throw new RpcError(
                  StatusCode.INTERNAL,
                  "client-stream call received " + responses.size() + " response frames");
            }
            out.complete(responses.get(0));
          } catch (Throwable t) {
            out.completeExceptionally(unwrap(t));
          } finally {
            if (call != 0) Rpc.release(rt, call);
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
          long rt = conn.runtime().nativeHandle();
          long call = 0;
          try {
            call =
                Rpc.open(
                    rt,
                    conn.nativeHandle(),
                    Rpc.PATTERN_BIDI_STREAM,
                    Rpc.Options.of(service, method, sessionId));
            sendAll(rt, call, requests);
            out.complete(drain(rt, call, responseType));
          } catch (Throwable t) {
            out.completeExceptionally(unwrap(t));
          } finally {
            if (call != 0) Rpc.release(rt, call);
          }
        });
    return out;
  }

  // -------------------------------------------------------------------------
  // Shared helpers
  // -------------------------------------------------------------------------

  /** Requests per {@link Rpc#sendMany} batch (bounds the native copy). */
  private static final int SEND_BATCH = 256;

  /**
   * Send every request in batches (one native call and as few writes as possible per batch), the
   * last ending the request stream; stops if the call is over.
   */
  private <Req> void sendAll(long rt, long call, List<Req> requests) {
    int n = requests.size();
    for (int start = 0; start < n; start += SEND_BATCH) {
      int end = Math.min(n, start + SEND_BATCH);
      List<byte[]> batch = new ArrayList<>(end - start);
      for (int i = start; i < end; i++) {
        batch.add(codec.encode(requests.get(i)));
      }
      if (!Rpc.sendMany(rt, call, batch, end == n)) {
        return; // the trailer says why
      }
    }
  }

  /** Read every response until the trailer; a non-OK trailer throws. */
  private <Resp> List<Resp> drain(long rt, long call, Class<Resp> responseType) {
    List<Resp> collected = new ArrayList<>();
    while (true) {
      Rpc.Recv r = Rpc.recvPolling(rt, call, () -> false); // bounded waits; interrupt cancels
      switch (r) {
        case Rpc.Recv.Data d -> {
          @SuppressWarnings("unchecked")
          Resp decoded = (Resp) codec.decode(d.payload(), responseType);
          collected.add(decoded);
        }
        case Rpc.Recv.Trailer t -> {
          RpcError e = statusError(t.status());
          if (e != null) {
            throw e;
          }
          return collected;
        }
        case Rpc.Recv.Timeout ignored -> {
          // recvPolling never returns Timeout
        }
      }
    }
  }

  /** The {@link RpcError} of a non-OK AEv1 status; {@code null} for OK. */
  static RpcError statusError(byte[] status) {
    if (status.length == 0) {
      return new RpcError(StatusCode.INTERNAL, "call already over");
    }
    RpcStatus st = AsterEnvelope.decodeRpcStatus(status);
    if (st.code() == RpcStatus.OK) {
      return null;
    }
    Map<String, String> details = new java.util.LinkedHashMap<>();
    for (int i = 0; i < Math.min(st.detailKeys().size(), st.detailValues().size()); i++) {
      details.put(st.detailKeys().get(i), st.detailValues().get(i));
    }
    return new RpcError(
        StatusCode.fromValue(st.code()), st.message() == null ? "" : st.message(), details);
  }

  private static RpcError rpcError(byte[] status) {
    RpcError e = statusError(status);
    return e != null ? e : new RpcError(StatusCode.UNKNOWN, "non-OK call with an OK status");
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
    private byte[] secretKey;
    String admissionCredentialJson = "";
    String admissionIidToken = "";
    boolean consumerAdmission = true;

    private Builder() {}

    /**
     * Whether to run the {@code aster.consumer_admission} handshake before the first call to a peer
     * (default true). Set false to dial {@code aster/1} directly — e.g. an open-gate Rust {@code
     * AsterServer}, which serves no consumer-admission ALPN.
     */
    public Builder consumerAdmission(boolean enabled) {
      this.consumerAdmission = enabled;
      return this;
    }

    public Builder config(AsterConfig config) {
      this.config = config;
      if (config != null && config.secretKey() != null && this.secretKey == null) {
        this.secretKey = config.secretKey();
      }
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

    /** 32-byte ed25519 seed for the local endpoint. Stabilises this consumer's peer id. */
    public Builder secretKey(byte[] secretKey) {
      this.secretKey = secretKey;
      return this;
    }

    /**
     * Load an {@code .aster-identity} TOML file and apply both the node secret key and a consumer
     * peer entry as the admission credential. Peer selection is by role — the first {@code
     * [[peers]]} with {@code role = "consumer"}, matching Python's {@code AsterClient(identity=…)}
     * default. Open-gate servers ignore the credential body; auth-mode servers verify it.
     */
    public Builder identity(Path path) {
      AsterIdentity id = AsterIdentity.load(path);
      if (id.nodeSecretKey() != null) {
        this.secretKey = id.nodeSecretKey();
      }
      Optional<AsterIdentity.PeerEntry> consumer = id.findByRole("consumer");
      if (consumer.isEmpty() && !id.peers().isEmpty()) {
        consumer = Optional.of(id.peers().get(0));
      }
      consumer.ifPresent(p -> this.admissionCredentialJson = AsterIdentity.credentialJson(p));
      return this;
    }

    public Builder identity(String path) {
      return identity(Path.of(path));
    }

    /** Override the peer name selection used by {@link #identity(Path)}. */
    public Builder identity(Path path, String peerName) {
      AsterIdentity id = AsterIdentity.load(path);
      if (id.nodeSecretKey() != null) {
        this.secretKey = id.nodeSecretKey();
      }
      id.findByName(peerName)
          .ifPresent(p -> this.admissionCredentialJson = AsterIdentity.credentialJson(p));
      return this;
    }

    /** Optional cloud IID token presented alongside the credential (default: empty). */
    public Builder iidToken(String token) {
      this.admissionIidToken = token == null ? "" : token;
      return this;
    }

    /**
     * Start the client's node ({@code aster_node_start}) from {@link #config} — relay mode, bind
     * address, secret key / key file, transports — and build the client on it.
     */
    public CompletableFuture<AsterClient> build() {
      site.aster.handle.IrohRuntime runtime = site.aster.handle.IrohRuntime.create();
      AsterConfig nodeConfig = config;
      byte[] key = secretKey != null && secretKey.length > 0 ? secretKey : null;
      List<byte[]> alpns =
          List.of(
              AsterServer.ASTER_ALPN.getBytes(java.nio.charset.StandardCharsets.UTF_8),
              ConsumerAdmissionHandler.ALPN_STRING.getBytes(
                  java.nio.charset.StandardCharsets.UTF_8));
      Builder self = this;
      return CompletableFuture.supplyAsync(
          () -> {
            try {
              long node = site.aster.ffi.NativeConfig.startNode(runtime, nodeConfig, alpns, 0, key);
              return new AsterClient(self, site.aster.handle.IrohEndpoint.ofNode(runtime, node));
            } catch (RuntimeException e) {
              runtime.close();
              throw e;
            }
          });
    }
  }
}
