package site.aster.client;

import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.Executor;
import java.util.function.BooleanSupplier;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.config.AsterIdentity;
import site.aster.ffi.AsterEnvelope;
import site.aster.ffi.Rpc;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.interceptors.CallContext;
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
 *
 * <p>The builder's {@link Builder#interceptors interceptors} run on every call pattern (see {@link
 * Interception}): request interceptors before sending, response interceptors on every response,
 * error interceptors (reverse order) on a failure. Unary and client-streaming calls additionally
 * run under {@code CircuitBreakerInterceptor} (per attempt) and {@code RetryInterceptor}
 * (idempotent calls, see {@link CallOptions#idempotent()}), and {@code DeadlineInterceptor} refuses
 * an expired call; {@link CallOptions#timeout()} is propagated to the server as the remaining
 * deadline of each attempt. Metadata an interceptor adds to {@code ctx.metadata()} goes out as
 * request headers. The auth / capability interceptors are frozen and unchanged (plan D3).
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

  /** The connection a call runs on; resolved (and so dialled) inside each attempt. */
  @FunctionalInterface
  interface ConnSource {
    IrohConnection get() throws Exception;
  }

  private ConnSource dial(NodeAddr target) {
    return () -> {
      try {
        return connect(target).get();
      } catch (java.util.concurrent.ExecutionException e) {
        throw e.getCause() instanceof Exception ex ? ex : e;
      }
    };
  }

  /** Make a unary RPC call against a SHARED-scoped service (sessionId=0). */
  public <Req, Resp> CompletableFuture<Resp> call(
      NodeAddr target, String service, String method, Req request, Class<Resp> responseType) {
    return call(target, service, method, request, responseType, CallOptions.DEFAULT);
  }

  /**
   * Make a unary RPC call with per-call {@link CallOptions}: the client's interceptors run around
   * it (deadline, circuit breaker, retry of idempotent calls, user interceptors; see {@link
   * Interception}). Cancelling the returned future cancels the native call (the server's handler
   * sees the cancel); the call still takes the pooled single-round-trip fast path.
   */
  public <Req, Resp> CompletableFuture<Resp> call(
      NodeAddr target,
      String service,
      String method,
      Req request,
      Class<Resp> responseType,
      CallOptions options) {
    return runUnary(
        dial(target), target.endpointId(), 0, service, method, request, responseType, options);
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
    return callServerStream(target, service, method, request, responseType, CallOptions.DEFAULT);
  }

  /** {@link #callServerStream} with per-call options; cancelling the future cancels the call. */
  public <Req, Resp> CompletableFuture<List<Resp>> callServerStream(
      NodeAddr target,
      String service,
      String method,
      Req request,
      Class<Resp> responseType,
      CallOptions options) {
    return runServerStream(
        dial(target), target.endpointId(), 0, service, method, request, responseType, options);
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
    return openServerStream(target, service, method, request, responseType, CallOptions.DEFAULT);
  }

  /**
   * {@link #openServerStream} with per-call options. The request interceptors run on {@code
   * request} when the call opens; the response interceptors on every {@code recv()}, the error
   * interceptors on a failing one.
   */
  public <Req, Resp> CompletableFuture<ServerStreamCall<Resp>> openServerStream(
      NodeAddr target,
      String service,
      String method,
      Req request,
      Class<Resp> responseType,
      CallOptions options) {
    return connect(target)
        .thenApplyAsync(
            conn ->
                openServerStreamOn(
                    conn, target.endpointId(), 0, service, method, request, responseType, options),
            callExecutor);
  }

  <Req, Resp> ServerStreamCall<Resp> openServerStreamOn(
      IrohConnection conn,
      String peer,
      int sessionId,
      String service,
      String method,
      Req request,
      Class<Resp> responseType,
      CallOptions options) {
    CallContext ctx =
        Interception.context(
            interceptors, service, method, sessionId, peer, "server_stream", options);
    Object req = request;
    if (ctx != null) {
      req = Interception.request(interceptors, ctx, request);
    }
    long rt = conn.runtime().nativeHandle();
    long call =
        Rpc.open(
            rt,
            conn.nativeHandle(),
            Rpc.PATTERN_SERVER_STREAM,
            streamOptions(service, method, sessionId, ctx, options));
    try {
      Rpc.send(rt, call, codec.encode(req), true);
    } catch (Throwable t) {
      Rpc.release(rt, call);
      throw reThrow(t);
    }
    return new ServerStreamCall<>(
        rt, call, codec, responseType, ctx == null ? null : new Intercepted(interceptors, ctx));
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
    return callClientStream(target, service, method, requests, responseType, CallOptions.DEFAULT);
  }

  /**
   * {@link #callClientStream} with per-call options. Like unary calls it runs under the full
   * interceptor lifecycle (breaker, retry of idempotent calls); the request interceptors run on
   * every request, each attempt.
   */
  public <Req, Resp> CompletableFuture<Resp> callClientStream(
      NodeAddr target,
      String service,
      String method,
      Iterable<Req> requests,
      Class<Resp> responseType,
      CallOptions options) {
    List<Req> materialized = materialize(requests);
    if (materialized.isEmpty()) {
      return CompletableFuture.failedFuture(
          new IllegalArgumentException(
              "callClientStream requires at least one request frame; the wire format delivers"
                  + " the first frame inline with the call to bootstrap the dispatcher"));
    }
    return runClientStream(
        dial(target), target.endpointId(), 0, service, method, materialized, responseType, options);
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
    return callBidiStream(target, service, method, requests, responseType, CallOptions.DEFAULT);
  }

  /** {@link #callBidiStream} with per-call options; cancelling the future cancels the call. */
  public <Req, Resp> CompletableFuture<List<Resp>> callBidiStream(
      NodeAddr target,
      String service,
      String method,
      Iterable<Req> requests,
      Class<Resp> responseType,
      CallOptions options) {
    List<Req> materialized = materialize(requests);
    if (materialized.isEmpty()) {
      return CompletableFuture.failedFuture(
          new IllegalArgumentException(
              "callBidiStream requires at least one request frame; the wire format delivers"
                  + " the first frame inline with the call to bootstrap the dispatcher"));
    }
    return runBidiBuffered(
        dial(target), target.endpointId(), 0, service, method, materialized, responseType, options);
  }

  /**
   * Open a true interleaved bidi-streaming call. Returns a {@link BidiCall} object the caller
   * drives directly via {@link BidiCall#send}, {@link BidiCall#recv}, {@link BidiCall#complete}.
   */
  public <Req, Resp> CompletableFuture<BidiCall<Req, Resp>> openBidiStream(
      NodeAddr target, String service, String method, Class<Resp> responseType) {
    return openBidiStream(target, service, method, responseType, CallOptions.DEFAULT);
  }

  /**
   * {@link #openBidiStream} with per-call options. The request interceptors run on every {@code
   * send}, the response interceptors on every {@code recv}, the error interceptors on a failing
   * one.
   */
  public <Req, Resp> CompletableFuture<BidiCall<Req, Resp>> openBidiStream(
      NodeAddr target,
      String service,
      String method,
      Class<Resp> responseType,
      CallOptions options) {
    return connect(target)
        .thenApplyAsync(
            conn ->
                openBidiStreamOn(
                    conn, target.endpointId(), 0, service, method, responseType, options),
            callExecutor);
  }

  <Req, Resp> BidiCall<Req, Resp> openBidiStreamOn(
      IrohConnection conn,
      String peer,
      int sessionId,
      String service,
      String method,
      Class<Resp> responseType,
      CallOptions options) {
    CallContext ctx =
        Interception.context(
            interceptors, service, method, sessionId, peer, "bidi_stream", options);
    long rt = conn.runtime().nativeHandle();
    long call =
        Rpc.open(
            rt,
            conn.nativeHandle(),
            Rpc.PATTERN_BIDI_STREAM,
            streamOptions(service, method, sessionId, ctx, options));
    return new BidiCall<Req, Resp>(
        rt, call, codec, responseType, ctx == null ? null : new Intercepted(interceptors, ctx));
  }

  private static Rpc.Options streamOptions(
      String service, String method, int sessionId, CallContext ctx, CallOptions options) {
    return new Rpc.Options(
        service,
        method,
        1,
        sessionId,
        Interception.deadlineMs(ctx, options),
        Interception.metadata(ctx, options));
  }

  /** A streaming call's interceptor chain and context (see {@link BidiCall}). */
  record Intercepted(List<Interceptor> chain, CallContext ctx) {}

  <Req, Resp> CompletableFuture<Resp> runUnary(
      ConnSource source,
      String peer,
      int sessionId,
      String service,
      String method,
      Req request,
      Class<Resp> responseType,
      CallOptions options) {
    InvocationFuture<Resp> out = new InvocationFuture<>();
    CallContext ctx =
        Interception.context(interceptors, service, method, sessionId, peer, "unary", options);
    callExecutor.execute(
        () -> {
          if (out.isCancelled()) {
            return;
          }
          try {
            Resp response =
                Interception.execute(
                    interceptors,
                    ctx,
                    options,
                    request,
                    false,
                    out::isCancelled,
                    (req, deadlineMs, metadata) ->
                        unaryAttempt(
                            out,
                            source.get(),
                            new Rpc.Options(service, method, 1, sessionId, deadlineMs, metadata),
                            req,
                            responseType));
            out.complete(response);
          } catch (Throwable t) {
            out.completeExceptionally(unwrap(t));
          }
        });
    return out;
  }

  /**
   * One unary attempt: encode, run on the pooled fast path, decode. Always cancellable (ABI 4.5):
   * the call carries a cancel token, and cancelling {@code out} cancels it ({@code
   * aster_cancel_token_cancel}), which aborts the native call like dropping the Rust future —
   * {@code FLAG_CANCEL} plus a reset of the pooled stream, so the server's handler sees the cancel.
   * A cancel before the call starts makes it return {@code CANCELLED} without sending.
   */
  private <Resp> Resp unaryAttempt(
      InvocationFuture<Resp> out,
      IrohConnection conn,
      Rpc.Options ro,
      Object request,
      Class<Resp> responseType)
      throws Exception {
    byte[] requestBytes = codec.encode(request);
    long rt = conn.runtime().nativeHandle();
    long token = Rpc.newCancelToken(rt);
    // Runs at once if out is already cancelled; a cancel after the call returned is a no-op.
    out.onCancel(() -> Rpc.cancelToken(rt, token));
    long probeT0 = site.aster.probe.AsterProbes.ENABLED ? System.nanoTime() : 0L;
    Rpc.UnaryResult result;
    try {
      result = Rpc.unary(rt, conn.nativeHandle(), ro, requestBytes, token);
    } catch (Throwable t) {
      Rpc.releaseCancelToken(rt, token); // in case the native call never took it
      throw t;
    } finally {
      out.clearOnCancel();
    }
    if (site.aster.probe.AsterProbes.ENABLED) {
      site.aster.probe.AsterProbes.recordClient(probeT0, System.nanoTime());
    }
    if (result.code() != RpcStatus.OK) {
      throw rpcError(result.status());
    }
    byte[] payload = result.payload();
    if (payload.length == 0) {
      throw new RpcError(StatusCode.INTERNAL, "OK trailer with no response frame");
    }
    @SuppressWarnings("unchecked")
    Resp decoded = (Resp) codec.decode(payload, responseType);
    return decoded;
  }

  <Req, Resp> CompletableFuture<List<Resp>> runServerStream(
      ConnSource source,
      String peer,
      int sessionId,
      String service,
      String method,
      Req request,
      Class<Resp> responseType,
      CallOptions options) {
    return runStreaming(
        source,
        peer,
        sessionId,
        service,
        method,
        "server_stream",
        Rpc.PATTERN_SERVER_STREAM,
        List.of(request),
        responseType,
        options);
  }

  <Req, Resp> CompletableFuture<List<Resp>> runBidiBuffered(
      ConnSource source,
      String peer,
      int sessionId,
      String service,
      String method,
      List<Req> requests,
      Class<Resp> responseType,
      CallOptions options) {
    return runStreaming(
        source,
        peer,
        sessionId,
        service,
        method,
        "bidi_stream",
        Rpc.PATTERN_BIDI_STREAM,
        requests,
        responseType,
        options);
  }

  /**
   * Server-stream and buffered bidi: no retry or breaker (as in Python), but the request
   * interceptors see every request, the response interceptors every response and the error
   * interceptors a failure (one that suppresses it ends the list early); the deadline propagates.
   */
  private <Resp> CompletableFuture<List<Resp>> runStreaming(
      ConnSource source,
      String peer,
      int sessionId,
      String service,
      String method,
      String pattern,
      int nativePattern,
      List<?> requests,
      Class<Resp> responseType,
      CallOptions options) {
    InvocationFuture<List<Resp>> out = new InvocationFuture<>();
    CallContext ctx =
        Interception.context(interceptors, service, method, sessionId, peer, pattern, options);
    callExecutor.execute(
        () -> {
          if (out.isCancelled()) {
            return;
          }
          List<Resp> collected = new ArrayList<>();
          try {
            List<Object> reqs = new ArrayList<>(requests);
            if (ctx != null) {
              reqs = Interception.requestEach(interceptors, ctx, reqs);
            }
            IrohConnection conn = source.get();
            long rt = conn.runtime().nativeHandle();
            long call =
                Rpc.open(
                    rt,
                    conn.nativeHandle(),
                    nativePattern,
                    streamOptions(service, method, sessionId, ctx, options));
            InvocationFuture.NativeCall nc = new InvocationFuture.NativeCall(rt, call);
            out.onCancel(nc::cancel);
            try {
              if (nativePattern == Rpc.PATTERN_SERVER_STREAM) {
                // one request, header and request in one write (send_payloads is not for these)
                Rpc.send(rt, call, codec.encode(reqs.get(0)), true);
              } else {
                sendAll(rt, call, reqs);
              }
              drain(rt, call, responseType, collected, ctx, out::isCancelled);
            } finally {
              out.clearOnCancel();
              nc.release();
            }
            out.complete(collected);
          } catch (Throwable t) {
            Throwable e = Interception.error(interceptors, ctx, t);
            if (e == null) {
              out.complete(collected);
            } else {
              out.completeExceptionally(unwrap(e));
            }
          }
        });
    return out;
  }

  <Req, Resp> CompletableFuture<Resp> runClientStream(
      ConnSource source,
      String peer,
      int sessionId,
      String service,
      String method,
      List<Req> requests,
      Class<Resp> responseType,
      CallOptions options) {
    InvocationFuture<Resp> out = new InvocationFuture<>();
    CallContext ctx =
        Interception.context(
            interceptors, service, method, sessionId, peer, "client_stream", options);
    callExecutor.execute(
        () -> {
          if (out.isCancelled()) {
            return;
          }
          try {
            Resp response =
                Interception.execute(
                    interceptors,
                    ctx,
                    options,
                    requests,
                    true,
                    out::isCancelled,
                    (reqs, deadlineMs, metadata) -> {
                      IrohConnection conn = source.get();
                      long rt = conn.runtime().nativeHandle();
                      long call =
                          Rpc.open(
                              rt,
                              conn.nativeHandle(),
                              Rpc.PATTERN_CLIENT_STREAM,
                              new Rpc.Options(service, method, 1, sessionId, deadlineMs, metadata));
                      InvocationFuture.NativeCall nc = new InvocationFuture.NativeCall(rt, call);
                      out.onCancel(nc::cancel);
                      try {
                        sendAll(rt, call, (List<?>) reqs);
                        List<Resp> responses = new ArrayList<>();
                        drain(rt, call, responseType, responses, null, out::isCancelled);
                        if (responses.size() != 1) {
                          throw new RpcError(
                              StatusCode.INTERNAL,
                              "client-stream call received "
                                  + responses.size()
                                  + " response frames");
                        }
                        return responses.get(0);
                      } finally {
                        out.clearOnCancel();
                        nc.release();
                      }
                    });
            out.complete(response);
          } catch (Throwable t) {
            out.completeExceptionally(unwrap(t));
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
  private void sendAll(long rt, long call, List<?> requests) {
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

  /**
   * Read every response until the trailer into {@code into}, applying the response interceptors
   * when {@code ctx} is set; a non-OK trailer throws. {@code stop} cancels the native call.
   */
  private <Resp> void drain(
      long rt,
      long call,
      Class<Resp> responseType,
      List<Resp> into,
      CallContext ctx,
      BooleanSupplier stop) {
    while (true) {
      Rpc.Recv r = Rpc.recvPolling(rt, call, stop); // bounded waits; interrupt cancels
      switch (r) {
        case Rpc.Recv.Data d -> {
          @SuppressWarnings("unchecked")
          Resp decoded = (Resp) codec.decode(d.payload(), responseType);
          if (ctx != null) {
            @SuppressWarnings("unchecked")
            Resp intercepted = (Resp) Interception.response(interceptors, ctx, decoded);
            decoded = intercepted;
          }
          into.add(decoded);
        }
        case Rpc.Recv.Trailer t -> {
          RpcError e = statusError(t.status());
          if (e != null) {
            throw e;
          }
          return;
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
