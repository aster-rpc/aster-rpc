package site.aster.expose;

import java.io.ByteArrayInputStream;
import java.io.InputStream;
import java.io.InterruptedIOException;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemoryLayout.PathElement;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohRuntime;
import site.aster.node.IrohNode;
import site.aster.upcall.Ucv1;
import site.aster.upcall.Ucv1.ExposeAuthorizeRequest;
import site.aster.upcall.Ucv1.ExposeHttpRequest;
import site.aster.upcall.Ucv1.ExposeHttpResponse;
import site.aster.upcall.Ucv1.ExposeRoutePolicyRequest;
import site.aster.upcall.Ucv1.ExposeRouteSpec;
import site.aster.upcall.Ucv1.HttpBody;
import site.aster.upcall.Ucv1.HttpHeader;
import site.aster.upcall.UpcallKind;
import site.aster.upcall.UpcallNative;
import site.aster.upcall.Upcalls;

/**
 * Publish local services to Aster peers, and consume a peer's (C ABI 4.2, upcall-design section 6).
 * An {@code Expose} is the registry of one node: services registered here are reached by relay
 * streams of connections you feed it ({@link #serve} owns the node's accept loop, {@link #handle}
 * takes one connection).
 *
 * <pre>{@code
 * try (Expose expose = Expose.create(node)) {
 *   // 1. A local TCP server (raw L4 splice): the optional authorizer admits peers.
 *   expose.exposeSocket("db", "127.0.0.1:5432", (service, peer, meta) -> allowed.contains(peer));
 *   // 2. A Java HTTP handler, one upcall per request (bodies stream with backpressure).
 *   expose.exposeHttp("app", req -> HttpResponse.text(200, "hi " + req.method()), authorizer);
 *   // 3. Act as an edge: a policy decides which routes peers may register.
 *   expose.serveRoutes((peer, metadata, specs) -> specs);
 *   expose.serve(List.of(ALPN));          // or expose.handle(connection) per connection
 * }
 * // Consumer side, on a connection to that ALPN:
 * RelayedResponse r = Expose.relayRequest(conn, "app", "GET", "/", null).get();
 * RouteRegistration reg = Expose.requestRoute(conn, specs, metadata).get();
 * }</pre>
 *
 * <p><b>Threads.</b> Callbacks run on virtual threads of the {@link Upcalls} table (a private one
 * unless you pass yours to {@link #create(IrohNode, Upcalls, HttpBodies)}), so they may block.
 * Native body I/O never blocks a carrier thread (see {@link HttpBodies}).
 *
 * <p><b>Failure modes.</b> The three callback kinds (authorize 48, HTTP 49, route policy 50) are
 * claimed by this registry on its table: a second {@code Expose} on the same table would replace
 * them. Authorize and route policy are decisions: with no usable answer (exception, timeout,
 * overload, close) the kind's failure mode applies, fail-closed unless you set {@code
 * upcalls().setPolicy(...)}. An HTTP handler without an answer becomes {@code 500} (exception),
 * {@code 501}, {@code 502}, {@code 503} or {@code 504} (timeout of the kind's policy: raise it for
 * long-polling handlers; it bounds the time to the response head only).
 *
 * <p><b>Admission.</b> {@link #exposeSocket} without an authorizer admits <b>every</b> peer that
 * reaches the node's ALPN: do that only when the node itself is access-controlled (Gate 0). {@link
 * #exposeHttp} without an authorizer likewise admits every peer.
 */
public final class Expose implements AutoCloseable {

  private static final System.Logger LOG = System.getLogger("aster.expose");

  // -- aster_http_foreign_options_t (48 bytes) and aster_route_spec_t (40 bytes) -------------

  public static final MemoryLayout HTTP_FOREIGN_OPTIONS_LAYOUT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), //          0
          ValueLayout.JAVA_INT.withName("flags"), //               4
          ValueLayout.JAVA_LONG.withName("inline_request_max"), // 8
          ValueLayout.JAVA_LONG.withName("max_request_body"), //   16
          ValueLayout.JAVA_LONG.withName("max_response_body"), //  24
          ValueLayout.JAVA_LONG.withName("max_chunk"), //          32
          ValueLayout.JAVA_INT.withName("response_buffer"), //     40
          ValueLayout.JAVA_INT.withName("reserved")); //           44 -> 48

  public static final MemoryLayout ROUTE_SPEC_LAYOUT =
      MemoryLayout.structLayout(
          IrohLibrary.ASTER_BYTES.withName("host"), //             0
          IrohLibrary.ASTER_BYTES.withName("service_id"), //       16
          ValueLayout.JAVA_INT.withName("port"), //                32
          ValueLayout.JAVA_INT.withName("protocol")); //           36 -> 40

  static {
    assertLayout(
        HTTP_FOREIGN_OPTIONS_LAYOUT,
        48,
        new String[] {
          "struct_size",
          "flags",
          "inline_request_max",
          "max_request_body",
          "max_response_body",
          "max_chunk",
          "response_buffer",
          "reserved"
        },
        new long[] {0, 4, 8, 16, 24, 32, 40, 44});
    assertLayout(
        ROUTE_SPEC_LAYOUT,
        40,
        new String[] {"host", "service_id", "port", "protocol"},
        new long[] {0, 16, 32, 36});
  }

  private static void assertLayout(
      MemoryLayout layout, long size, String[] fields, long[] offsets) {
    if (layout.byteSize() != size) {
      throw new AssertionError(layout + " size drift: " + layout.byteSize() + " != " + size);
    }
    for (int i = 0; i < fields.length; i++) {
      long at = layout.byteOffset(PathElement.groupElement(fields[i]));
      if (at != offsets[i]) {
        throw new AssertionError(fields[i] + " offset drift: " + at + " != " + offsets[i]);
      }
    }
  }

  private static final ValueLayout.OfInt I = UpcallNative.I;
  private static final ValueLayout.OfLong J = UpcallNative.J;
  private static final java.lang.foreign.AddressLayout A = UpcallNative.A;
  private static final MemoryLayout B = IrohLibrary.ASTER_BYTES;
  private static final MemoryLayout BL = IrohLibrary.ASTER_BYTES_LIST;

  private static final MethodHandle NEW =
      UpcallNative.handle("aster_expose_new", FunctionDescriptor.of(I, J, J, A));
  private static final MethodHandle FREE =
      UpcallNative.handle("aster_expose_free", FunctionDescriptor.of(I, J, J));
  private static final MethodHandle UNEXPOSE =
      UpcallNative.handle("aster_expose_unexpose", FunctionDescriptor.of(I, J, J, B));
  private static final MethodHandle HANDLE_CONNECTION =
      UpcallNative.handle("aster_expose_handle_connection", FunctionDescriptor.of(I, J, J, J));
  private static final MethodHandle SERVE =
      UpcallNative.handle("aster_expose_serve", FunctionDescriptor.of(I, J, J, J, BL, J, A));
  private static final MethodHandle SOCKET =
      UpcallNative.handle("aster_expose_socket", FunctionDescriptor.of(I, J, J, B, B, J));
  private static final MethodHandle OPTIONS_INIT =
      UpcallNative.handle("aster_http_foreign_options_init", FunctionDescriptor.of(I, A));
  private static final MethodHandle HTTP_FOREIGN =
      UpcallNative.handle("aster_expose_http_foreign", FunctionDescriptor.of(I, J, J, J, J, B, A));
  private static final MethodHandle ROUTE_POLICY_FOREIGN =
      UpcallNative.handle("aster_expose_route_policy_foreign", FunctionDescriptor.of(I, J, J, J));
  private static final MethodHandle RELAY_REQUEST =
      UpcallNative.handle("aster_expose_relay_request", FunctionDescriptor.of(I, J, J, B, J, A));
  private static final MethodHandle REQUEST_ROUTE =
      UpcallNative.handle(
          "aster_expose_request_route", FunctionDescriptor.of(I, J, J, A, J, B, J, A));

  private static final MethodHandle RELAY_OPTIONS_INIT =
      UpcallNative.handle("aster_http_relay_options_init", FunctionDescriptor.of(I, A));
  private static final MethodHandle RELAY_STREAM =
      UpcallNative.handle("aster_expose_relay_stream", FunctionDescriptor.of(I, J, J, B, A, A, A));

  private record HttpService(HttpHandler handler, long maxChunk) {}

  private final IrohNode node;
  private final IrohRuntime runtime;
  private final long handle;
  private final Upcalls upcalls;
  private final HttpBodies bodies;
  private final boolean owned;
  private final AtomicBoolean closed = new AtomicBoolean();
  private final Map<String, ExposeAuthorizer> authorizers = new ConcurrentHashMap<>();
  private final Map<String, HttpService> httpServices = new ConcurrentHashMap<>();
  private final Set<String> exposed = ConcurrentHashMap.newKeySet();
  private volatile RoutePolicy routePolicy;
  private boolean routesInstalled;
  private boolean httpInstalled;

  private Expose(IrohNode node, long handle, Upcalls upcalls, HttpBodies bodies, boolean owned) {
    this.node = node;
    this.runtime = node.runtime();
    this.handle = handle;
    this.upcalls = upcalls;
    this.bodies = bodies;
    this.owned = owned;
  }

  /** A registry on {@code node} with a private upcall table and body table (closed with it). */
  public static Expose create(IrohNode node) {
    Upcalls up = Upcalls.create(node.runtime());
    HttpBodies bodies = null;
    try {
      bodies = HttpBodies.create(node.runtime());
      return build(node, up, bodies, true);
    } catch (RuntimeException e) {
      if (bodies != null) {
        bodies.close();
      }
      up.close();
      throw e;
    }
  }

  /**
   * A registry on {@code node} answering on your {@code upcalls} and {@code bodies} (both on the
   * node's runtime; you close them, after this registry). It takes over the table's authorize, HTTP
   * and route-policy kinds.
   */
  public static Expose create(IrohNode node, Upcalls upcalls, HttpBodies bodies) {
    if (upcalls.runtime() != node.runtime() || bodies.runtime() != node.runtime()) {
      throw new IllegalArgumentException("upcalls and bodies must belong to the node's runtime");
    }
    return build(node, upcalls, bodies, false);
  }

  private static Expose build(IrohNode node, Upcalls up, HttpBodies bodies, boolean owned) {
    long h =
        UpcallNative.callForLong(
            "aster_expose_new",
            (a, out) -> (int) NEW.invoke(node.runtime().nativeHandle(), node.nodeHandle(), out));
    Expose e = new Expose(node, h, up, bodies, owned);
    up.handle(UpcallKind.EXPOSE_AUTHORIZE, e::authorizeUpcall);
    return e;
  }

  public IrohNode node() {
    return node;
  }

  public long nativeHandle() {
    return handle;
  }

  /** The table the callbacks are answered on (set per-kind policies here). */
  public Upcalls upcalls() {
    return upcalls;
  }

  /** The body-handle table of the HTTP exchanges. */
  public HttpBodies bodies() {
    return bodies;
  }

  // -- providers -----------------------------------------------------------------------------

  /**
   * Expose the TCP server at {@code addr} ({@code "127.0.0.1:8080"}, a loopback server you run) as
   * {@code serviceId}: peers' streams are spliced to it. {@code authorize} (may be {@code null})
   * admits peers once per {@code (connection, service)}; {@code null} admits every peer that
   * reaches the node's ALPN. Re-registering an id replaces it.
   */
  public Expose exposeSocket(String serviceId, String addr, ExposeAuthorizer authorize) {
    checkOpen();
    requireId(serviceId);
    if (authorize != null) {
      authorizers.put(serviceId, authorize);
    }
    try (Arena a = Arena.ofConfined()) {
      int rc =
          (int)
              SOCKET.invoke(
                  runtime.nativeHandle(),
                  handle,
                  DataPlane.utf8(a, serviceId),
                  DataPlane.utf8(a, addr),
                  authorize == null ? 0L : upcalls.nativeHandle());
      UpcallNative.check(rc, "aster_expose_socket");
    } catch (Throwable t) {
      authorizers.remove(serviceId);
      throw UpcallNative.rethrow(t);
    }
    if (authorize == null) {
      authorizers.remove(serviceId);
    }
    httpServices.remove(serviceId);
    exposed.add(serviceId);
    return this;
  }

  public Expose exposeSocket(String serviceId, InetSocketAddress addr, ExposeAuthorizer authorize) {
    String host = addr.getAddress().getHostAddress();
    return exposeSocket(
        serviceId,
        (host.contains(":") ? "[" + host + "]" : host) + ":" + addr.getPort(),
        authorize);
  }

  /**
   * {@link #exposeHttp(String, HttpHandler, ExposeHttpOptions, ExposeAuthorizer)} with defaults.
   */
  public Expose exposeHttp(String serviceId, HttpHandler handler, ExposeAuthorizer authorize) {
    return exposeHttp(serviceId, handler, ExposeHttpOptions.defaults(), authorize);
  }

  /**
   * Expose {@code handler} as {@code serviceId}: one {@code expose_http} upcall per request, bodies
   * moving through {@link #bodies()} (see {@link HttpRequest} / {@link HttpResponse}). {@code
   * authorize} (may be {@code null}: every peer that reaches the ALPN is admitted) runs once per
   * {@code (connection, service)}. Re-registering an id replaces it.
   */
  public Expose exposeHttp(
      String serviceId,
      HttpHandler handler,
      ExposeHttpOptions options,
      ExposeAuthorizer authorize) {
    checkOpen();
    requireId(serviceId);
    if (handler == null) {
      throw new NullPointerException("handler");
    }
    ExposeHttpOptions o = options == null ? ExposeHttpOptions.defaults() : options;
    installHttp();
    HttpService previous =
        httpServices.put(serviceId, new HttpService(handler, o.effectiveMaxChunk()));
    ExposeAuthorizer previousAuth =
        authorize == null ? authorizers.remove(serviceId) : authorizers.put(serviceId, authorize);
    try (Arena a = Arena.ofConfined()) {
      MemorySegment opts = a.allocate(HTTP_FOREIGN_OPTIONS_LAYOUT);
      UpcallNative.check((int) OPTIONS_INIT.invoke(opts), "aster_http_foreign_options_init");
      opts.set(ValueLayout.JAVA_LONG, 8, o.inlineRequestMax());
      opts.set(ValueLayout.JAVA_LONG, 16, o.maxRequestBody());
      opts.set(ValueLayout.JAVA_LONG, 24, o.maxResponseBody());
      opts.set(ValueLayout.JAVA_LONG, 32, o.maxChunk());
      opts.set(ValueLayout.JAVA_INT, 40, o.responseBuffer());
      UpcallNative.check(
          (int)
              HTTP_FOREIGN.invoke(
                  runtime.nativeHandle(),
                  handle,
                  upcalls.nativeHandle(),
                  bodies.nativeHandle(),
                  DataPlane.utf8(a, serviceId),
                  opts),
          "aster_expose_http_foreign");
    } catch (Throwable t) {
      if (previous == null) {
        httpServices.remove(serviceId);
      } else {
        httpServices.put(serviceId, previous);
      }
      if (previousAuth == null) {
        authorizers.remove(serviceId);
      } else {
        authorizers.put(serviceId, previousAuth);
      }
      throw UpcallNative.rethrow(t);
    }
    exposed.add(serviceId);
    return this;
  }

  /**
   * Act as an edge: accept route registrations from peers ({@link #requestRoute}), granting what
   * {@code policy} returns. A second call replaces the policy. No policy means no edge: there is no
   * grant-everything default. With no usable answer the kind's failure mode applies to the whole
   * request (fail-closed grants nothing); the routes land in an internal router (the C ABI has no
   * public edge listener).
   */
  public synchronized Expose serveRoutes(RoutePolicy policy) {
    checkOpen();
    if (policy == null) {
      throw new NullPointerException("policy");
    }
    if (!routesInstalled) {
      upcalls.handle(UpcallKind.EXPOSE_ROUTE_POLICY, this::routePolicyUpcall);
      try {
        UpcallNative.check(
            (int)
                ROUTE_POLICY_FOREIGN.invoke(runtime.nativeHandle(), handle, upcalls.nativeHandle()),
            "aster_expose_route_policy_foreign");
      } catch (Throwable t) {
        upcalls.removeHandler(UpcallKind.EXPOSE_ROUTE_POLICY);
        throw UpcallNative.rethrow(t);
      }
      routesInstalled = true;
    }
    routePolicy = policy;
    return this;
  }

  /** Stop exposing {@code serviceId} (idempotent); streams already being served finish. */
  public void unexpose(String serviceId) {
    checkOpen();
    try (Arena a = Arena.ofConfined()) {
      UpcallNative.check(
          (int) UNEXPOSE.invoke(runtime.nativeHandle(), handle, DataPlane.utf8(a, serviceId)),
          "aster_expose_unexpose");
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
    authorizers.remove(serviceId);
    httpServices.remove(serviceId);
    exposed.remove(serviceId);
  }

  // -- delivering connections ----------------------------------------------------------------

  /**
   * Own the node's accept loop for expose: accept inbound connections and feed each whose ALPN is
   * in {@code alpns} (empty = every ALPN) to this registry. Completes exceptionally with the accept
   * error when the node closes; cancelling the future only stops the completion event, so close the
   * node to stop the loop. Do not combine with the node's own accept loop: use {@link #handle}
   * there.
   */
  public CompletableFuture<Void> serve(List<byte[]> alpns) {
    checkOpen();
    return UpcallNative.submit(
            runtime,
            "aster_expose_serve",
            (a, out) -> {
              MemorySegment list = a.allocate(BL);
              if (alpns.isEmpty()) {
                list.set(A, 0, MemorySegment.NULL);
                list.set(J, 8, 0L);
              } else {
                MemorySegment arr = a.allocate(B, alpns.size());
                for (int i = 0; i < alpns.size(); i++) {
                  arr.asSlice(i * B.byteSize(), B.byteSize())
                      .copyFrom(DataPlane.bytes(a, alpns.get(i)));
                }
                list.set(A, 0, arr);
                list.set(J, 8, (long) alpns.size());
              }
              return (int)
                  SERVE.invoke(runtime.nativeHandle(), node.nodeHandle(), handle, list, 0L, out);
            })
        .thenApply(r -> null);
  }

  /**
   * Feed one inbound connection to the registry so its relay streams reach the exposed services.
   * Call once per connection, before the peer opens streams on it; the connection stays yours.
   */
  public void handle(IrohConnection connection) {
    checkOpen();
    try {
      UpcallNative.check(
          (int) HANDLE_CONNECTION.invoke(runtime.nativeHandle(), handle, connection.nativeHandle()),
          "aster_expose_handle_connection");
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  // -- consumer side -------------------------------------------------------------------------

  /** A buffered {@code HTTP/1.1} relay with no extra headers; see the full overload. */
  public static CompletableFuture<RelayedResponse> relayRequest(
      IrohConnection connection, String serviceId, String method, String uri, byte[] body) {
    return relayRequest(connection, serviceId, method, uri, List.of(), body, HttpVersion.HTTP_1_1);
  }

  /**
   * Relay one buffered HTTP request to the service {@code serviceId} the peer exposed on {@code
   * connection}, and collect the whole response (the consumer needs no {@link Expose}). Completes
   * with the response for <b>any</b> HTTP status, including the {@code 5xx} a failing handler maps
   * to; fails on a transport failure, a refused request, or a response the peer aborted mid-body.
   * Header names are lowercased; {@code body} may be {@code null}.
   */
  public static CompletableFuture<RelayedResponse> relayRequest(
      IrohConnection connection,
      String serviceId,
      String method,
      String uri,
      List<HttpHeader> headers,
      byte[] body,
      HttpVersion version) {
    IrohRuntime rt = connection.runtime();
    byte[] c4;
    try {
      List<HttpHeader> lower = new ArrayList<>(headers.size() + 1);
      boolean hasLength = false;
      for (HttpHeader h : headers) {
        String name = h.name().toLowerCase(Locale.ROOT);
        hasLength |= name.equals("content-length");
        lower.add(new HttpHeader(name, h.value()));
      }
      // The whole body is in hand, so declare its length: the handler's side then knows whether it
      // travels inline and can refuse an oversized one up front.
      if (!hasLength && body != null) {
        lower.add(
            new HttpHeader(
                "content-length",
                Integer.toString(body.length).getBytes(StandardCharsets.US_ASCII)));
      }
      c4 =
          Ucv1.encodeExposeHttpRequest(
              new ExposeHttpRequest(
                  serviceId,
                  null,
                  0,
                  version.code,
                  method,
                  uri,
                  lower,
                  new HttpBody.Inline(body == null ? new byte[0] : body)));
    } catch (RuntimeException e) {
      return CompletableFuture.failedFuture(e);
    }
    return UpcallNative.submit(
            rt,
            "aster_expose_relay_request",
            (a, out) ->
                (int)
                    RELAY_REQUEST.invoke(
                        rt.nativeHandle(),
                        connection.nativeHandle(),
                        DataPlane.bytes(a, c4),
                        0L,
                        out))
        .thenApply(
            r -> {
              ExposeHttpResponse c5 = Ucv1.decodeExposeHttpResponse(r.data());
              if (!(c5.body() instanceof HttpBody.Inline inline)) {
                throw new IrohException("relay response body is not inline");
              }
              return new RelayedResponse(c5.status(), c5.headers(), inline.body());
            });
  }

  /**
   * Ask {@code connection}'s peer (an edge using {@link #serveRoutes}) to route {@code specs} to
   * us, attaching {@code metadata} for its policy. The edge keeps the granted routes until the
   * returned registration is closed. A refusal of the whole request fails the future.
   */
  public static CompletableFuture<RouteRegistration> requestRoute(
      IrohConnection connection, List<RouteSpec> specs, byte[] metadata) {
    IrohRuntime rt = connection.runtime();
    List<RouteSpec> requested = List.copyOf(specs);
    byte[] meta = metadata == null ? new byte[0] : metadata;
    CompletableFuture<DataPlane.Result> raw =
        UpcallNative.submit(
            rt,
            "aster_expose_request_route",
            (a, out) -> {
              MemorySegment arr = MemorySegment.NULL;
              if (!requested.isEmpty()) {
                arr = a.allocate(ROUTE_SPEC_LAYOUT, requested.size());
                for (int i = 0; i < requested.size(); i++) {
                  RouteSpec s = requested.get(i);
                  MemorySegment one = arr.asSlice(i * ROUTE_SPEC_LAYOUT.byteSize());
                  one.asSlice(0, 16).copyFrom(DataPlane.utf8(a, s.host()));
                  one.asSlice(16, 16).copyFrom(DataPlane.utf8(a, s.serviceId()));
                  one.set(ValueLayout.JAVA_INT, 32, s.port());
                  one.set(ValueLayout.JAVA_INT, 36, s.protocol().code);
                }
              }
              return (int)
                  REQUEST_ROUTE.invoke(
                      rt.nativeHandle(),
                      connection.nativeHandle(),
                      arr,
                      (long) requested.size(),
                      DataPlane.bytes(a, meta),
                      0L,
                      out);
            });
    // A cancelled result future must still free the registration the edge granted.
    CompletableFuture<RouteRegistration> out = new CompletableFuture<>();
    raw.whenComplete(
        (r, err) -> {
          if (err != null) {
            out.completeExceptionally(err);
            return;
          }
          RouteRegistration reg = null;
          try {
            List<Integer> granted = new ArrayList<>();
            for (long i : Ucv1.decodeExposeRouteGrant(r.data())) {
              granted.add((int) i);
            }
            reg = new RouteRegistration(rt, r.event().handle(), requested, granted);
            if (!out.complete(reg)) {
              reg.close();
            }
          } catch (RuntimeException e) {
            if (reg != null) {
              reg.close();
            }
            out.completeExceptionally(e);
          }
        });
    return out;
  }

  /** The two ends of a streaming relay (see {@link #relayStreaming}). */
  public record RelayCall(RelayUpload upload, CompletableFuture<RelayResponse> response) {}

  /**
   * Start a <b>streaming</b> HTTP exchange with the service {@code serviceId} the peer exposed on
   * {@code connection} and return both ends at once: write the request body to {@code upload} and
   * {@link RelayUpload#finish} it while {@code response} completes with the head (full duplex: the
   * head can arrive before the upload is done, an early 413, and the upload carries on after it).
   * You own both: close the upload (an unfinished one is aborted) and the response. A refusal or
   * transport failure fails {@code response} with an {@link IrohException}: {@code
   * PERMISSION_DENIED} for a denied relay, {@code NOT_FOUND} for an unknown service, and so on.
   * Cancelling {@code response} before the head stops the wait and frees its handle. Synchronous
   * argument errors throw.
   */
  public static RelayCall relayStreaming(
      IrohConnection connection,
      String serviceId,
      String method,
      String uri,
      List<HttpHeader> headers,
      RelayOptions options) {
    IrohRuntime rt = connection.runtime();
    RelayOptions o = options == null ? RelayOptions.defaults() : options;
    List<HttpHeader> lower = new ArrayList<>(headers.size());
    for (HttpHeader h : headers) {
      lower.add(new HttpHeader(h.name().toLowerCase(Locale.ROOT), h.value()));
    }
    byte[] c4 =
        Ucv1.encodeExposeHttpRequest(
            new ExposeHttpRequest(
                serviceId,
                null,
                0,
                HttpVersion.HTTP_1_1.code,
                method,
                uri,
                lower,
                HttpBody.STREAM));
    long upload;
    long response;
    try (Arena a = Arena.ofConfined()) {
      MemorySegment opts = a.allocate(RelayOptions.LAYOUT);
      UpcallNative.check((int) RELAY_OPTIONS_INIT.invoke(opts), "aster_http_relay_options_init");
      opts.set(ValueLayout.JAVA_LONG, 8, o.maxChunk());
      opts.set(ValueLayout.JAVA_INT, 16, o.uploadCapacity());
      MemorySegment outUp = a.allocate(J);
      MemorySegment outResp = a.allocate(J);
      UpcallNative.check(
          (int)
              RELAY_STREAM.invoke(
                  rt.nativeHandle(),
                  connection.nativeHandle(),
                  DataPlane.bytes(a, c4),
                  opts,
                  outUp,
                  outResp),
          "aster_expose_relay_stream");
      upload = outUp.get(J, 0);
      response = outResp.get(J, 0);
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
    long maxChunk = o.effectiveMaxChunk();
    CompletableFuture<RelayResponse> head = new CompletableFuture<>();
    Thread.ofVirtual()
        .name("aster-relay-head-" + response)
        .start(
            () -> {
              try {
                RelayResponse r =
                    RelayResponse.awaitHead(rt, response, maxChunk, head::isCancelled);
                if (!head.complete(r)) {
                  r.close();
                }
              } catch (InterruptedException e) {
                head.cancel(false);
              } catch (Throwable t) {
                head.completeExceptionally(t);
              }
            });
    return new RelayCall(new RelayUpload(rt, upload, maxChunk), head);
  }

  /**
   * A full-duplex streaming relay with the request body fed from {@code body} ({@code null}: no
   * body), which is closed when the upload ends. The returned future completes as soon as the
   * response <b>head</b> is in, possibly before the body is uploaded; the upload carries on in the
   * background (on a virtual thread) until it is done, the handler stops reading, or the response
   * is closed before its body ended (which resets an unfinished upload; a body read to its clean
   * end leaves the upload to finish).
   *
   * <p>If {@code body} throws, the request is reset (the handler sees a failed body, never a
   * truncated one): before the head that exception fails the future; after it, it is logged under
   * {@code aster.expose} (the response itself is already in hand). Any other failure is the
   * relay's: see {@link #relayStreaming}. Cancelling the future before the head abandons the
   * exchange.
   */
  public static CompletableFuture<RelayResponse> relayStream(
      IrohConnection connection,
      String serviceId,
      String method,
      String uri,
      List<HttpHeader> headers,
      java.io.InputStream body,
      RelayOptions options) {
    RelayCall call;
    try {
      call = relayStreaming(connection, serviceId, method, uri, headers, options);
    } catch (RuntimeException e) {
      closeQuietly(body);
      return CompletableFuture.failedFuture(e);
    }
    java.util.concurrent.atomic.AtomicReference<Throwable> sourceFailure =
        new java.util.concurrent.atomic.AtomicReference<>();
    AtomicBoolean headIn = new AtomicBoolean();
    AtomicBoolean pumpDone = new AtomicBoolean();
    Thread pump =
        Thread.ofVirtual()
            .name("aster-relay-upload")
            .unstarted(
                () -> {
                  try {
                    pumpUpload(call.upload(), body, sourceFailure, headIn);
                  } finally {
                    pumpDone.set(true);
                  }
                });
    CompletableFuture<RelayResponse> out = new CompletableFuture<>();
    call.response()
        .whenComplete(
            (r, err) -> {
              if (err == null) {
                headIn.set(true);
                r.onAbandon(
                    () -> {
                      if (!pumpDone.get()) {
                        pump.interrupt();
                      }
                    });
                if (!out.complete(r)) {
                  r.close();
                }
                return;
              }
              Throwable cause = sourceFailure.get();
              out.completeExceptionally(cause != null ? cause : err);
              pump.interrupt();
            });
    out.whenComplete(
        (r, err) -> {
          if (out.isCancelled()) {
            call.response().cancel(false);
            pump.interrupt();
          }
        });
    pump.start();
    return out;
  }

  private static void pumpUpload(
      RelayUpload upload,
      java.io.InputStream body,
      java.util.concurrent.atomic.AtomicReference<Throwable> sourceFailure,
      AtomicBoolean headIn) {
    try {
      if (body != null) {
        byte[] buf = new byte[64 * 1024];
        int n;
        while ((n = body.read(buf)) >= 0) {
          if (n > 0) {
            upload.write(buf, 0, n, null);
          }
        }
      }
      upload.finish();
    } catch (HttpBodyException e) {
      if (e.exchangeOver()) {
        // The handler stopped reading (it may have answered already), or the exchange ended.
        LOG.log(System.Logger.Level.DEBUG, "relay upload ended: {0}", e.toString());
      } else {
        LOG.log(System.Logger.Level.WARNING, "relay upload failed: {0}", e.toString());
        abortQuietly(upload, "upload failed");
      }
    } catch (InterruptedIOException e) {
      abortQuietly(upload, "cancelled");
    } catch (Throwable t) {
      // The caller's own body source failed.
      sourceFailure.set(t);
      abortQuietly(upload, "request body source failed");
      if (headIn.get()) {
        LOG.log(
            System.Logger.Level.WARNING,
            "relay request body failed after the response head: " + t,
            t);
      }
    } finally {
      closeQuietly(body);
      upload.close();
    }
  }

  private static void abortQuietly(RelayUpload upload, String message) {
    try {
      upload.abort(message);
    } catch (Exception ignored) {
      // already over
    }
  }

  private static void closeQuietly(java.io.Closeable c) {
    if (c != null) {
      try {
        c.close();
      } catch (Exception ignored) {
        // best effort
      }
    }
  }

  // -- lifecycle -----------------------------------------------------------------------------

  /**
   * Stop exposing every service, free the registry (once) and, for a private table, close the
   * upcall table (in-flight handlers are interrupted and their streamed bodies aborted) and the
   * body table. Connections already fed to the registry keep their routes' policy only as long as
   * the table answers: after close an edge's policy has no answer, so it fails closed.
   */
  @Override
  public void close() {
    if (!closed.compareAndSet(false, true)) {
      return;
    }
    for (String id : new ArrayList<>(exposed)) {
      try (Arena a = Arena.ofConfined()) {
        UNEXPOSE.invoke(runtime.nativeHandle(), handle, DataPlane.utf8(a, id));
      } catch (Throwable ignored) {
        // runtime already closed
      }
    }
    exposed.clear();
    try {
      FREE.invoke(runtime.nativeHandle(), handle);
    } catch (Throwable ignored) {
      // runtime already closed
    }
    upcalls.removeHandler(UpcallKind.EXPOSE_AUTHORIZE);
    upcalls.removeHandler(UpcallKind.EXPOSE_HTTP);
    upcalls.removeHandler(UpcallKind.EXPOSE_ROUTE_POLICY);
    if (owned) {
      upcalls.close();
      bodies.close();
    }
  }

  // -- upcall handlers -----------------------------------------------------------------------

  private byte[] authorizeUpcall(byte[] request, long deadlineUnixMs) throws Exception {
    ExposeAuthorizeRequest r = Ucv1.decodeExposeAuthorizeRequest(request);
    ExposeAuthorizer a = authorizers.get(r.serviceId());
    if (a == null) {
      // An HTTP service registered without an authorizer admits everyone; anything else is not
      // ours.
      return Ucv1.encodeDecision(httpServices.containsKey(r.serviceId()));
    }
    return Ucv1.encodeDecision(a.authorize(r.serviceId(), r.peerId(), r.metadata()));
  }

  private synchronized void installHttp() {
    if (!httpInstalled) {
      upcalls.handleInvocation(UpcallKind.EXPOSE_HTTP, this::serveHttp);
      httpInstalled = true;
    }
  }

  private byte[] routePolicyUpcall(byte[] request, long deadlineUnixMs) throws Exception {
    RoutePolicy policy = routePolicy;
    if (policy == null) {
      throw new IllegalStateException("no route policy");
    }
    ExposeRoutePolicyRequest r = Ucv1.decodeExposeRoutePolicyRequest(request);
    List<RouteSpec> specs = new ArrayList<>(r.specs().size());
    for (ExposeRouteSpec s : r.specs()) {
      specs.add(
          new RouteSpec(
              s.host(), s.port(), RouteSpec.Protocol.fromCode(s.protocol()), s.serviceId()));
    }
    List<RouteSpec> granted = policy.grant(r.peerId(), r.metadata(), List.copyOf(specs));
    Set<RouteSpec> want = new HashSet<>(granted == null ? List.of() : granted);
    // A granted spec that was not requested is a bug in the policy: no usable answer.
    if (!new HashSet<>(specs).containsAll(want)) {
      throw new IllegalArgumentException("route policy granted a spec that was not requested");
    }
    List<Long> indexes = new ArrayList<>();
    for (int i = 0; i < specs.size(); i++) {
      if (want.contains(specs.get(i))) {
        indexes.add((long) i);
      }
    }
    return Ucv1.encodeExposeRouteGrant(indexes);
  }

  private void serveHttp(Upcalls.Invocation inv) throws Exception {
    ExposeHttpRequest req = Ucv1.decodeExposeHttpRequest(inv.payload());
    HttpService svc = httpServices.get(req.serviceId());
    if (svc == null) {
      inv.respond(Upcalls.STATUS_UNIMPLEMENTED, new byte[0]);
      return;
    }
    boolean inline = req.body() instanceof HttpBody.Inline;
    InputStream body =
        req.body() instanceof HttpBody.Inline in
            ? new ByteArrayInputStream(in.body())
            : bodies.requestBody(req.streamId());
    HttpRequest request =
        new HttpRequest(
            req.serviceId(),
            req.peerId(),
            req.streamId(),
            HttpVersion.fromCode(req.version()),
            req.method(),
            req.uri(),
            req.headers(),
            body,
            inline);
    HttpResponse resp = svc.handler().handle(request);
    if (resp == null) {
      throw new IllegalStateException("HttpHandler returned null");
    }
    HttpBody respBody = resp.isStreaming() ? HttpBody.STREAM : new HttpBody.Inline(resp.rawBody());
    byte[] c5 =
        Ucv1.encodeExposeHttpResponse(
            new ExposeHttpResponse(resp.status(), resp.headers(), respBody));
    if (!inv.respond(Upcalls.STATUS_OK, c5) || !resp.isStreaming()) {
      // Rust stopped waiting (the exchange is closed), or the body was inline: done.
      return;
    }
    streamBody(req, resp, svc);
  }

  /** After a streamed head went out: write the body, then finish (or abort on failure). */
  private void streamBody(ExposeHttpRequest req, HttpResponse resp, HttpService svc) {
    long id = req.streamId();
    ResponseBody out = new ResponseBody(bodies, id, svc.maxChunk());
    try {
      resp.writer().write(out);
      bodies.finish(id, out.trailers());
    } catch (HttpBodyException e) {
      if (e.exchangeOver()) {
        // The consumer went away (or the exchange is over): nothing to tell.
        LOG.log(
            System.Logger.Level.DEBUG, "streamed response on {0} ended: {1}", req.serviceId(), e);
      } else {
        LOG.log(
            System.Logger.Level.WARNING,
            "streamed response on {0} failed: {1}",
            req.serviceId(),
            e.toString());
        abortQuietly(id, e.toString());
      }
    } catch (InterruptedIOException | InterruptedException e) {
      // Cancelled by Rust or the table closing.
      Thread.currentThread().interrupt();
      abortQuietly(id, "cancelled");
    } catch (Throwable t) {
      LOG.log(
          System.Logger.Level.WARNING,
          "HTTP handler for " + req.serviceId() + " failed mid-body: " + t,
          t);
      abortQuietly(id, t.getClass().getSimpleName() + ": " + t.getMessage());
    }
  }

  private void abortQuietly(long streamId, String message) {
    try {
      bodies.abort(streamId, message);
    } catch (Exception ignored) {
      // already over: the relay closed the exchange
    }
  }

  // -- helpers -------------------------------------------------------------------------------

  private void checkOpen() {
    if (closed.get()) {
      throw new IllegalStateException("Expose is closed");
    }
  }

  private static void requireId(String serviceId) {
    if (serviceId == null || serviceId.isEmpty()) {
      throw new IllegalArgumentException("serviceId must be non-empty");
    }
  }
}
