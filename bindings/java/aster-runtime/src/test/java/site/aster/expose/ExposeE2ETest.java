package site.aster.expose;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import site.aster.config.AsterConfig;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.handle.IrohRuntime;
import site.aster.node.IrohNode;
import site.aster.upcall.FailureMode;
import site.aster.upcall.Ucv1;
import site.aster.upcall.Ucv1.HttpHeader;
import site.aster.upcall.UpcallKind;
import site.aster.upcall.Upcalls;

/**
 * C ABI 4.2 expose end to end over real QUIC (loopback, relays off): the wiring of a socket target
 * with a Java authorize callback, Java HTTP handlers with inline and streamed bodies, Java handler
 * failures as statuses, a Java route policy, handle lifetimes and the body-call error mapping.
 *
 * <p>The relay's behaviour is tested once in Rust, in {@code ffi/tests/expose.rs}: authorizer
 * absence and failure modes ({@code socket_target_with_and_without_a_foreign_authorizer}, {@code
 * authorize_without_an_answer_follows_the_failure_mode}), size caps ({@code request_body_limits},
 * {@code response_over_the_cap_fails_the_consumer}), 501 and mid-body aborts ({@code
 * handler_failures_become_5xx_statuses}, {@code aborted_streamed_response_fails_the_consumer}),
 * consumer cancellation ({@code consumer_cancel_before_the_head_cancels_the_upcall}, {@code
 * consumer_cancel_mid_stream_makes_writes_cancelled}), route-policy timeouts ({@code
 * route_policy_grant_and_deny}), {@code unexpose_stops_the_service}, {@code
 * handle_connection_feeds_accepted_connections}, {@code request_route_validates_its_arguments} and
 * {@code relay_request_validates_its_arguments}.
 */
final class ExposeE2ETest {

  private static final String ALPN = "aster-expose/java-test";
  private static final long T = 30;

  static <V> V await(CompletableFuture<V> f) throws Exception {
    return f.get(T, TimeUnit.SECONDS);
  }

  static byte[] pattern(int n) {
    byte[] b = new byte[n];
    for (int i = 0; i < n; i++) {
      b[i] = (byte) (i % 251);
    }
    return b;
  }

  static void waitFor(String what, java.util.function.BooleanSupplier cond)
      throws InterruptedException {
    long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(T);
    while (!cond.getAsBoolean()) {
      if (System.nanoTime() > end) {
        throw new AssertionError("timed out waiting for " + what);
      }
      Thread.sleep(10);
    }
  }

  static IrohNode startNode() throws Exception {
    AsterConfig cfg = AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build();
    return IrohNode.start(cfg, List.of(ALPN.getBytes(StandardCharsets.UTF_8)), null)
        .get(T, TimeUnit.SECONDS);
  }

  /** Two nodes: {@code server} exposes (accept loop owned by expose), {@code client} consumes. */
  static final class Rig implements AutoCloseable {
    final IrohNode server;
    final IrohNode client;
    final Expose expose;
    final IrohConnection conn;
    final CompletableFuture<Void> serving;

    Rig() throws Exception {
      server = startNode();
      client = startNode();
      expose = Expose.create(server);
      serving = expose.serve(List.of(ALPN.getBytes(StandardCharsets.UTF_8)));
      var addr = server.nodeAddr();
      for (int i = 0; i < 100 && addr.directAddresses().isEmpty(); i++) {
        Thread.sleep(50);
        addr = server.nodeAddr();
      }
      assertFalse(addr.directAddresses().isEmpty(), "server has a direct address");
      conn =
          await(
              IrohEndpoint.ofNode(client.runtime(), client.nodeHandle())
                  .connectNodeAddrAsync(addr, ALPN));
    }

    String clientId() {
      return client.nodeId();
    }

    @Override
    public void close() {
      expose.close();
      conn.close();
      client.close();
      server.close();
    }
  }

  private static ServerSocket target() throws IOException {
    ServerSocket s = new ServerSocket(0, 50, InetAddress.getLoopbackAddress());
    s.setSoTimeout(15_000);
    return s;
  }

  private static InetSocketAddress addr(ServerSocket s) {
    return new InetSocketAddress(InetAddress.getLoopbackAddress(), s.getLocalPort());
  }

  // ── socket targets + authorize ───────────────────────────────────────────

  @Test
  void socketTargetAuthorizeAllowAndDeny() throws Exception {
    try (Rig rig = new Rig();
        ServerSocket target = target()) {
      AtomicBoolean allow = new AtomicBoolean(false);
      List<String> seen = new CopyOnWriteArrayList<>();
      rig.expose.exposeSocket(
          "sock",
          addr(target),
          (service, peer, metadata) -> {
            seen.add(service + "/" + peer);
            return allow.get();
          });

      // Denied: the relay fails and nothing reaches the target.
      assertThrows(
          ExecutionException.class,
          () -> await(Expose.relayRequest(rig.conn, "sock", "GET", "/", null)));
      assertTrue(seen.contains("sock/" + rig.clientId()), "authorizer saw the peer: " + seen);
      target.setSoTimeout(300);
      assertThrows(SocketTimeoutException.class, target::accept);

      // Allowed: the stream is spliced to the target (it sees a TCP connection; the framing is
      // not HTTP/1.1, so the relay call only ends once the target hangs up).
      allow.set(true);
      target.setSoTimeout(15_000);
      CompletableFuture<RelayedResponse> pending =
          Expose.relayRequest(rig.conn, "sock", "GET", "/", null);
      try (Socket spliced = target.accept()) {
        assertNotNull(spliced);
      }
      try {
        pending.get(T, TimeUnit.SECONDS);
      } catch (ExecutionException expected) {
        // the target hung up on a non-HTTP stream
      }
    }
  }

  // ── HTTP handlers ────────────────────────────────────────────────────────

  @Test
  void inlineRequestAndResponseReachTheHandlerAndBack() throws Exception {
    try (Rig rig = new Rig()) {
      AtomicReference<HttpRequest> seen = new AtomicReference<>();
      AtomicReference<byte[]> body = new AtomicReference<>();
      List<String> authorized = new CopyOnWriteArrayList<>();
      rig.expose.exposeHttp(
          "app",
          req -> {
            seen.set(req);
            body.set(req.readBody());
            return HttpResponse.of(201, "pong:".getBytes(StandardCharsets.UTF_8))
                .header("x-echo", req.header("x-test"));
          },
          (s, p, m) -> authorized.add(s + "/" + p));

      RelayedResponse r =
          await(
              Expose.relayRequest(
                  rig.conn,
                  "app",
                  "PUT",
                  "/a/b?q=%20",
                  List.of(
                      new HttpHeader("X-Test", "v1".getBytes(StandardCharsets.UTF_8)),
                      new HttpHeader("accept", "*/*".getBytes(StandardCharsets.UTF_8))),
                  "xyz".getBytes(StandardCharsets.UTF_8),
                  HttpVersion.HTTP_1_1));
      assertEquals(201, r.status());
      assertEquals("pong:", r.bodyText());
      assertEquals("v1", new String(r.header("X-Echo"), StandardCharsets.UTF_8));

      HttpRequest req = seen.get();
      assertEquals("app", req.serviceId());
      assertEquals(rig.clientId(), req.peerId());
      assertTrue(req.streamId() >= 1);
      assertEquals("PUT", req.method());
      assertEquals("/a/b?q=%20", req.uri());
      assertEquals("v1", req.headerText("x-test"));
      assertEquals("*/*", req.headerText("ACCEPT"));
      assertArrayEquals("xyz".getBytes(StandardCharsets.UTF_8), body.get());
      assertTrue(req.inline());
      assertEquals(List.of("app/" + rig.clientId()), authorized);
      waitFor("exchanges to close", () -> rig.expose.bodies().openCount() == 0);
    }
  }

  @Test
  void streamedRequestAndResponseWithBackpressure() throws Exception {
    try (Rig rig = new Rig()) {
      byte[] up = pattern(300_000);
      byte[] down = pattern(1_000_000);
      AtomicReference<byte[]> got = new AtomicReference<>();
      AtomicBoolean inline = new AtomicBoolean(true);
      // A tiny chunk and buffer force the writer to wait on the consumer many times.
      rig.expose.exposeHttp(
          "app",
          req -> {
            inline.set(req.inline());
            got.set(req.readBody());
            return HttpResponse.streaming(
                    200,
                    out -> {
                      for (int at = 0; at < down.length; at += 50_000) {
                        out.write(down, at, Math.min(50_000, down.length - at));
                      }
                      out.trailer("x-done", "yes".getBytes(StandardCharsets.US_ASCII));
                    })
                .header("content-type", "application/octet-stream");
          },
          ExposeHttpOptions.defaults()
              .withInlineRequestMax(1024)
              .withMaxChunk(4096)
              .withResponseBuffer(2),
          null);

      RelayedResponse r = await(Expose.relayRequest(rig.conn, "app", "POST", "/up", up));
      assertEquals(200, r.status());
      assertArrayEquals(down, r.body());
      assertArrayEquals(up, got.get());
      assertFalse(inline.get(), "a body over inlineRequestMax streams");
      waitFor("exchanges to close", () -> rig.expose.bodies().openCount() == 0);
    }
  }

  @Test
  void handlerFailuresBecome5xxStatuses() throws Exception {
    try (Rig rig = new Rig()) {
      rig.expose.exposeHttp(
          "app",
          req -> {
            switch (req.uri()) {
              case "/throw":
                throw new IllegalStateException("kaboom");
              case "/null":
                return null;
              default:
                return HttpResponse.text(404, "nope");
            }
          },
          null);
      assertEquals(
          500, await(Expose.relayRequest(rig.conn, "app", "GET", "/throw", null)).status());
      assertEquals(500, await(Expose.relayRequest(rig.conn, "app", "GET", "/null", null)).status());
      RelayedResponse nf = await(Expose.relayRequest(rig.conn, "app", "GET", "/x", null));
      assertEquals(404, nf.status());
      assertEquals("nope", nf.bodyText());
      assertEquals(
          "text/plain; charset=utf-8",
          new String(nf.header("content-type"), StandardCharsets.UTF_8));

      // A handler slower than the kind's timeout answers 504 and is interrupted.
      CountDownLatch interrupted = new CountDownLatch(1);
      rig.expose.exposeHttp(
          "slow",
          req -> {
            try {
              Thread.sleep(20_000);
            } catch (InterruptedException e) {
              interrupted.countDown();
              throw e;
            }
            return HttpResponse.text(200, "late");
          },
          null);
      rig.expose
          .upcalls()
          .setPolicy(UpcallKind.EXPOSE_HTTP, Duration.ofMillis(300), FailureMode.FAIL_CLOSED);
      assertEquals(504, await(Expose.relayRequest(rig.conn, "slow", "GET", "/", null)).status());
      assertTrue(interrupted.await(T, TimeUnit.SECONDS));
      waitFor("exchanges to close", () -> rig.expose.bodies().openCount() == 0);
    }
  }

  @Test
  void invalidTrailersAbortTheResponse() throws Exception {
    try (Rig rig = new Rig()) {
      rig.expose.exposeHttp(
          "app",
          req ->
              HttpResponse.streaming(
                  200,
                  out -> {
                    out.write(pattern(100));
                    out.trailer("x-bad", new byte[] {1}); // not a valid header value
                  }),
          null);
      assertThrows(
          ExecutionException.class,
          () -> await(Expose.relayRequest(rig.conn, "app", "GET", "/", null)));
      waitFor("exchanges to close", () -> rig.expose.bodies().openCount() == 0);
    }
  }

  // ── route policy + request_route ─────────────────────────────────────────

  private static List<RouteSpec> specs() {
    return List.of(
        new RouteSpec("a.example", 443, RouteSpec.Protocol.HTTP, "web"),
        new RouteSpec("", 8080, RouteSpec.Protocol.TCP, "db"),
        new RouteSpec("b.example", 0, RouteSpec.Protocol.HTTP, "api"));
  }

  @Test
  void routePolicyGrantsAndDenies() throws Exception {
    try (Rig rig = new Rig()) {
      AtomicReference<String> mode = new AtomicReference<>("grant");
      List<String> seen = new CopyOnWriteArrayList<>();
      rig.expose.serveRoutes(
          (peer, metadata, requested) -> {
            seen.add(
                peer + "/" + new String(metadata, StandardCharsets.UTF_8) + "/" + requested.size());
            switch (mode.get()) {
              case "grant":
                return List.of(requested.get(0), requested.get(2));
              case "deny":
                return List.of();
              case "unrequested":
                return List.of(new RouteSpec("evil.example", 0, RouteSpec.Protocol.HTTP, "x"));
              default:
                throw new IllegalStateException("policy backend down");
            }
          });

      byte[] token = "tok".getBytes(StandardCharsets.UTF_8);
      try (RouteRegistration reg = await(Expose.requestRoute(rig.conn, specs(), token))) {
        assertEquals(List.of(0, 2), reg.grantedIndexes());
        assertEquals(List.of(specs().get(0), specs().get(2)), reg.granted());
        assertEquals(List.of(rig.clientId() + "/tok/3"), seen);
      }

      mode.set("deny");
      try (RouteRegistration reg = await(Expose.requestRoute(rig.conn, specs(), null))) {
        assertEquals(List.of(), reg.grantedIndexes());
      }

      // A malformed answer (a spec nobody asked for) and an exception are "no usable answer":
      // fail-closed grants nothing.
      for (String m : List.of("unrequested", "throw")) {
        mode.set(m);
        try (RouteRegistration reg = await(Expose.requestRoute(rig.conn, specs(), null))) {
          assertEquals(List.of(), reg.grantedIndexes(), m);
        }
      }

      // serveRoutes again replaces the policy.
      rig.expose.serveRoutes((peer, metadata, requested) -> requested);
      try (RouteRegistration reg = await(Expose.requestRoute(rig.conn, specs(), null))) {
        assertEquals(List.of(0, 1, 2), reg.grantedIndexes());
      }
    }
  }

  // ── lifecycle ────────────────────────────────────────────────────────────

  @Test
  void closeInterruptsStreamingHandlersAndIsIdempotent() throws Exception {
    Rig rig = new Rig();
    try {
      CountDownLatch streaming = new CountDownLatch(1);
      CountDownLatch ended = new CountDownLatch(1);
      AtomicBoolean interrupted = new AtomicBoolean();
      rig.expose.exposeHttp(
          "app",
          req ->
              HttpResponse.streaming(
                  200,
                  out -> {
                    try {
                      streaming.countDown();
                      while (true) {
                        out.write(new byte[1024]);
                        Thread.sleep(5);
                      }
                    } catch (InterruptedException | java.io.InterruptedIOException e) {
                      interrupted.set(true);
                      throw e;
                    } finally {
                      ended.countDown();
                    }
                  }),
          null);
      CompletableFuture<RelayedResponse> pending =
          Expose.relayRequest(rig.conn, "app", "GET", "/", null);
      assertTrue(streaming.await(T, TimeUnit.SECONDS));
      rig.expose.close();
      assertTrue(ended.await(T, TimeUnit.SECONDS), "the streaming handler ends on close");
      assertTrue(interrupted.get());
      rig.expose.close(); // second close is a no-op
      assertThrows(IllegalStateException.class, () -> rig.expose.unexpose("app"));
      assertThrows(
          IllegalStateException.class,
          () -> rig.expose.exposeHttp("x", r -> HttpResponse.text(200, ""), null));
      try {
        pending.get(T, TimeUnit.SECONDS);
      } catch (ExecutionException expected) {
        // the exchange was aborted
      }
    } finally {
      rig.close();
    }
  }

  @Test
  void aSharedTableOutlivesTheRegistry() throws Exception {
    IrohNode node = startNode();
    try (Upcalls up = Upcalls.create(node.runtime());
        HttpBodies bodies = HttpBodies.create(node.runtime())) {
      Expose expose = Expose.create(node, up, bodies);
      expose.exposeHttp("app", req -> HttpResponse.text(200, "x"), null);
      expose.close();
      expose.close();
      // The caller's tables are still usable and are closed by the caller (twice is harmless).
      assertEquals(0, bodies.openCount());
      up.setPolicy(UpcallKind.EXPOSE_HTTP, Duration.ofSeconds(1), FailureMode.FAIL_CLOSED);
      bodies.close();
      bodies.close();
    } finally {
      node.close();
    }
  }

  // ── HttpBodies ───────────────────────────────────────────────────────────

  @Test
  void bodyCallsOnUnknownExchangesAndHandles() throws Exception {
    try (IrohRuntime rt = IrohRuntime.create()) {
      HttpBodies bodies = HttpBodies.create(rt);
      assertEquals(0, bodies.openCount());
      byte[] chunk = {1, 2, 3};
      assertEquals(
          HttpBodyException.Kind.UNKNOWN_STREAM,
          assertThrows(
                  HttpBodyException.class, () -> bodies.read(424_242, 16, Duration.ofMillis(50)))
              .kind());
      assertEquals(
          HttpBodyException.Kind.UNKNOWN_STREAM,
          assertThrows(
                  HttpBodyException.class,
                  () -> bodies.write(424_242, chunk, Duration.ofMillis(50)))
              .kind());
      assertEquals(
          HttpBodyException.Kind.UNKNOWN_STREAM,
          assertThrows(HttpBodyException.class, () -> bodies.tryWrite(424_242, chunk)).kind());
      assertEquals(
          HttpBodyException.Kind.UNKNOWN_STREAM,
          assertThrows(
                  HttpBodyException.class,
                  () -> bodies.writeBlocking(424_242, chunk, Duration.ofMillis(50)))
              .kind());
      assertEquals(
          HttpBodyException.Kind.UNKNOWN_STREAM,
          assertThrows(HttpBodyException.class, () -> bodies.finish(424_242, null)).kind());
      HttpBodyException trailers =
          assertThrows(
              HttpBodyException.class,
              () -> bodies.finish(424_242, List.of(new HttpHeader("x-t", new byte[] {1}))));
      assertTrue(
          trailers.exchangeOver() || trailers.kind() == HttpBodyException.Kind.INVALID_TRAILERS);
      HttpBodyException abort =
          assertThrows(HttpBodyException.class, () -> bodies.abort(424_242, "why"));
      assertEquals(1, abort.code());
      assertTrue(abort.exchangeOver());
      assertThrows(IllegalArgumentException.class, () -> bodies.read(1, 0, null));

      bodies.close();
      bodies.close(); // double free is a no-op
      assertEquals(
          HttpBodyException.Kind.UNKNOWN_HANDLE,
          assertThrows(HttpBodyException.class, () -> bodies.read(1, 16, Duration.ofMillis(50)))
              .kind());
    }
  }

  @Test
  void layoutsMatchTheHeader() {
    // ffi/aster_ffi.h: aster_http_foreign_options_t 48 bytes, aster_route_spec_t 40 bytes.
    assertEquals(48, Expose.HTTP_FOREIGN_OPTIONS_LAYOUT.byteSize());
    assertEquals(40, Expose.ROUTE_SPEC_LAYOUT.byteSize());
    assertEquals(HttpBodyException.Kind.CANCELLED, HttpBodyException.Kind.fromCode(2));
    assertEquals(HttpBodyException.Kind.UNKNOWN_HANDLE, HttpBodyException.Kind.fromCode(-2));
    assertEquals(HttpBodyException.Kind.INVALID_ARGUMENT, HttpBodyException.Kind.fromCode(-1));
    assertEquals(HttpBodyException.Kind.UNKNOWN, HttpBodyException.Kind.fromCode(77));
    assertEquals(9, HttpBodyException.Kind.BLOCKING_ON_RUNTIME.code);
    assertEquals(
        List.of(), Ucv1.decodeExposeRouteGrant(Ucv1.encodeExposeRouteGrant(new ArrayList<>())));
  }
}
