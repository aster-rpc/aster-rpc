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
import java.util.concurrent.atomic.AtomicInteger;
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
 * C ABI 4.2 expose end to end over real QUIC (loopback, relays off): socket targets with an
 * authorize callback (allow, deny, throw, timeout and its failure modes), HTTP handlers with inline
 * and streamed bodies (backpressure, abort, consumer cancellation), route policy, the buffered
 * relay and request_route, and handle lifetimes. Ports the cases of {@code ffi/tests/expose.rs}.
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

  @Test
  void socketTargetWithoutAuthorizerAdmitsEveryPeerAndAsksNothing() throws Exception {
    try (Rig rig = new Rig();
        ServerSocket target = target()) {
      AtomicInteger asked = new AtomicInteger();
      // The kind-48 handler is Expose's; a second service proves nothing is asked for "open".
      rig.expose.exposeSocket(
          "guarded",
          addr(target),
          (s, p, m) -> {
            asked.incrementAndGet();
            return false;
          });
      rig.expose.exposeSocket("open", addr(target), null);
      CompletableFuture<RelayedResponse> pending =
          Expose.relayRequest(rig.conn, "open", "GET", "/", null);
      try (Socket spliced = target.accept()) {
        assertNotNull(spliced);
      }
      try {
        pending.get(T, TimeUnit.SECONDS);
      } catch (ExecutionException expected) {
        // see above
      }
      assertEquals(0, asked.get());
    }
  }

  @Test
  void authorizeTimeoutIsFailClosedAndCancelsTheCallback() throws Exception {
    try (Rig rig = new Rig();
        ServerSocket target = target()) {
      rig.expose
          .upcalls()
          .setPolicy(UpcallKind.EXPOSE_AUTHORIZE, Duration.ofMillis(200), FailureMode.FAIL_CLOSED);
      CountDownLatch interrupted = new CountDownLatch(1);
      rig.expose.exposeSocket(
          "slow",
          addr(target),
          (s, p, m) -> {
            try {
              Thread.sleep(20_000);
            } catch (InterruptedException e) {
              interrupted.countDown();
              throw e;
            }
            return true;
          });
      assertThrows(
          ExecutionException.class,
          () -> await(Expose.relayRequest(rig.conn, "slow", "GET", "/", null)));
      assertTrue(interrupted.await(T, TimeUnit.SECONDS), "the callback is interrupted on timeout");
      target.setSoTimeout(300);
      assertThrows(SocketTimeoutException.class, target::accept);
      waitFor("a CANCEL event", () -> rig.expose.upcalls().cancelCount() >= 1);
    }
  }

  @Test
  void authorizeCancelCarriesItsReason() throws Exception {
    try (Rig rig = new Rig();
        ServerSocket target = target()) {
      rig.expose
          .upcalls()
          .setPolicy(UpcallKind.EXPOSE_AUTHORIZE, Duration.ofMillis(200), FailureMode.FAIL_CLOSED);
      rig.expose.exposeSocket("slow", addr(target), (s, p, m) -> true);
      AtomicInteger reason = new AtomicInteger();
      CountDownLatch done = new CountDownLatch(1);
      // Replace the dispatcher with a raw invocation handler to see the cancel reason.
      rig.expose
          .upcalls()
          .handleInvocation(
              UpcallKind.EXPOSE_AUTHORIZE,
              inv -> {
                try {
                  Thread.sleep(20_000);
                } catch (InterruptedException e) {
                  reason.set(inv.cancelReason());
                  done.countDown();
                  throw e;
                }
              });
      assertThrows(
          ExecutionException.class,
          () -> await(Expose.relayRequest(rig.conn, "slow", "GET", "/", null)));
      assertTrue(done.await(T, TimeUnit.SECONDS));
      assertEquals(Upcalls.CANCEL_TIMEOUT, reason.get());
    }
  }

  @Test
  void authorizeExceptionFollowsTheFailureMode() throws Exception {
    try (Rig rig = new Rig();
        ServerSocket target = target()) {
      rig.expose.exposeSocket(
          "boom",
          addr(target),
          (s, p, m) -> {
            throw new IllegalStateException("backend down");
          });
      // Fail-closed (default): an exception denies.
      assertThrows(
          ExecutionException.class,
          () -> await(Expose.relayRequest(rig.conn, "boom", "GET", "/", null)));
      target.setSoTimeout(300);
      assertThrows(SocketTimeoutException.class, target::accept);

      // Fail-open: an exception admits.
      rig.expose
          .upcalls()
          .setPolicy(UpcallKind.EXPOSE_AUTHORIZE, Duration.ofSeconds(5), FailureMode.FAIL_OPEN);
      target.setSoTimeout(15_000);
      CompletableFuture<RelayedResponse> pending =
          Expose.relayRequest(rig.conn, "boom", "GET", "/", null);
      try (Socket spliced = target.accept()) {
        assertNotNull(spliced);
      }
      try {
        pending.get(T, TimeUnit.SECONDS);
      } catch (ExecutionException expected) {
        // see above
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
  void requestBodyOverTheCapFailsTheReadAndAnswers413() throws Exception {
    try (Rig rig = new Rig()) {
      rig.expose.exposeHttp(
          "app",
          req -> HttpResponse.text(200, "unreachable"),
          ExposeHttpOptions.defaults().withMaxRequestBody(1000),
          null);
      // A declared length over the cap is refused without an upcall.
      RelayedResponse r = await(Expose.relayRequest(rig.conn, "app", "POST", "/", pattern(5000)));
      assertEquals(413, r.status());
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
  void aKindWithoutAHandlerAnswers501() throws Exception {
    try (Rig rig = new Rig()) {
      rig.expose.exposeHttp("app", req -> HttpResponse.text(200, "ok"), null);
      rig.expose.upcalls().removeHandler(UpcallKind.EXPOSE_HTTP);
      assertEquals(501, await(Expose.relayRequest(rig.conn, "app", "GET", "/", null)).status());
    }
  }

  @Test
  void aStreamedResponseThatFailsMidBodyFailsTheConsumer() throws Exception {
    try (Rig rig = new Rig()) {
      rig.expose.exposeHttp(
          "app",
          req ->
              HttpResponse.streaming(
                  200,
                  out -> {
                    out.write(pattern(1000));
                    throw new IllegalStateException("source broke");
                  }),
          null);
      // The relay resets the stream: the consumer sees a failure, never a truncated body.
      assertThrows(
          ExecutionException.class,
          () -> await(Expose.relayRequest(rig.conn, "app", "GET", "/", null)));
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

  @Test
  void responseOverTheCapAbortsTheStreamAndFailsTheConsumer() throws Exception {
    try (Rig rig = new Rig()) {
      AtomicReference<HttpBodyException.Kind> kind = new AtomicReference<>();
      rig.expose.exposeHttp(
          "app",
          req ->
              HttpResponse.streaming(
                  200,
                  out -> {
                    try {
                      for (int i = 0; i < 100; i++) {
                        out.write(pattern(1000));
                      }
                    } catch (HttpBodyException e) {
                      kind.set(e.kind());
                      throw e;
                    }
                  }),
          ExposeHttpOptions.defaults().withMaxResponseBody(5000),
          null);
      assertThrows(
          ExecutionException.class,
          () -> await(Expose.relayRequest(rig.conn, "app", "GET", "/", null)));
      waitFor("the cap to surface", () -> kind.get() != null);
      assertEquals(HttpBodyException.Kind.TOO_LARGE, kind.get());
    }
  }

  @Test
  void consumerLeavingBeforeTheHeadCancelsTheHandler() throws Exception {
    try (Rig rig = new Rig()) {
      CountDownLatch started = new CountDownLatch(1);
      AtomicInteger reason = new AtomicInteger();
      CountDownLatch cancelled = new CountDownLatch(1);
      rig.expose.exposeHttp("app", req -> HttpResponse.text(200, "unused"), null);
      // Raw handler so the test can read the cancel reason.
      rig.expose
          .upcalls()
          .handleInvocation(
              UpcallKind.EXPOSE_HTTP,
              inv -> {
                started.countDown();
                try {
                  Thread.sleep(30_000);
                } catch (InterruptedException e) {
                  reason.set(inv.cancelReason());
                  cancelled.countDown();
                  throw e;
                }
              });
      CompletableFuture<RelayedResponse> pending =
          Expose.relayRequest(rig.conn, "app", "GET", "/", null);
      assertTrue(started.await(T, TimeUnit.SECONDS));
      rig.conn.close();
      assertThrows(ExecutionException.class, () -> pending.get(T, TimeUnit.SECONDS));
      assertTrue(cancelled.await(T, TimeUnit.SECONDS), "the handler is interrupted");
      assertEquals(Upcalls.CANCEL_ABANDONED, reason.get());
      waitFor("exchanges to close", () -> rig.expose.bodies().openCount() == 0);
    }
  }

  @Test
  void consumerLeavingMidStreamMakesWritesCancelled() throws Exception {
    try (Rig rig = new Rig()) {
      AtomicReference<HttpBodyException.Kind> kind = new AtomicReference<>();
      CountDownLatch streaming = new CountDownLatch(1);
      rig.expose.exposeHttp(
          "app",
          req ->
              HttpResponse.streaming(
                  200,
                  out -> {
                    // Flood the exchange: the bounded buffer fills, so a write is parked when the
                    // consumer disappears and wakes with CANCELLED.
                    byte[] chunk = new byte[256 * 1024];
                    long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(T);
                    try {
                      while (System.nanoTime() < end) {
                        out.write(chunk);
                        streaming.countDown();
                      }
                    } catch (HttpBodyException e) {
                      kind.set(e.kind());
                      throw e;
                    }
                  }),
          null);
      CompletableFuture<RelayedResponse> pending =
          Expose.relayRequest(rig.conn, "app", "GET", "/", null);
      assertTrue(streaming.await(T, TimeUnit.SECONDS));
      rig.conn.close();
      assertThrows(ExecutionException.class, () -> pending.get(T, TimeUnit.SECONDS));
      waitFor("the writer to notice", () -> kind.get() != null);
      assertEquals(HttpBodyException.Kind.CANCELLED, kind.get());
      waitFor("exchanges to close", () -> rig.expose.bodies().openCount() == 0);
    }
  }

  @Test
  void unexposeStopsTheServiceAndReRegisteringBringsItBack() throws Exception {
    try (Rig rig = new Rig()) {
      rig.expose.exposeHttp("app", req -> HttpResponse.text(200, "up"), null);
      assertEquals("up", await(Expose.relayRequest(rig.conn, "app", "GET", "/", null)).bodyText());
      rig.expose.unexpose("app");
      rig.expose.unexpose("app"); // idempotent
      rig.expose.unexpose("never-exposed");
      assertThrows(
          ExecutionException.class,
          () -> await(Expose.relayRequest(rig.conn, "app", "GET", "/", null)));
      rig.expose.exposeHttp("app", req -> HttpResponse.text(200, "again"), null);
      assertEquals(
          "again", await(Expose.relayRequest(rig.conn, "app", "GET", "/", null)).bodyText());
    }
  }

  @Test
  void handleConnectionFeedsAcceptedConnections() throws Exception {
    // No serve loop: the node's endpoint accepts and the connection is fed by hand.
    IrohNode server = startNode();
    IrohNode client = startNode();
    try (Expose expose = Expose.create(server)) {
      expose.exposeHttp("app", req -> HttpResponse.text(200, "fed"), null);
      var addr = server.nodeAddr();
      for (int i = 0; i < 100 && addr.directAddresses().isEmpty(); i++) {
        Thread.sleep(50);
        addr = server.nodeAddr();
      }
      CompletableFuture<IrohConnection> accepted =
          IrohEndpoint.ofNode(server.runtime(), server.nodeHandle()).acceptAsync();
      IrohConnection conn =
          await(
              IrohEndpoint.ofNode(client.runtime(), client.nodeHandle())
                  .connectNodeAddrAsync(addr, ALPN));
      IrohConnection inbound = await(accepted);
      expose.handle(inbound);
      assertEquals("fed", await(Expose.relayRequest(conn, "app", "GET", "/", null)).bodyText());
      conn.close();
      inbound.close();
    } finally {
      client.close();
      server.close();
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
  void routePolicyGrantsDeniesAndFailsClosed() throws Exception {
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
              case "throw":
                throw new IllegalStateException("policy backend down");
              default:
                Thread.sleep(20_000); // "silent"
                return requested;
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

      // No answer in time: the failure mode decides; fail-open grants every route.
      rig.expose
          .upcalls()
          .setPolicy(UpcallKind.EXPOSE_ROUTE_POLICY, Duration.ofMillis(300), FailureMode.FAIL_OPEN);
      mode.set("silent");
      try (RouteRegistration reg = await(Expose.requestRoute(rig.conn, specs(), null))) {
        assertEquals(List.of(0, 1, 2), reg.grantedIndexes());
      }
      rig.expose
          .upcalls()
          .setPolicy(
              UpcallKind.EXPOSE_ROUTE_POLICY, Duration.ofMillis(300), FailureMode.FAIL_CLOSED);
      try (RouteRegistration reg = await(Expose.requestRoute(rig.conn, specs(), null))) {
        assertEquals(List.of(), reg.grantedIndexes());
      }

      // serveRoutes again replaces the policy.
      rig.expose.serveRoutes((peer, metadata, requested) -> requested);
      try (RouteRegistration reg = await(Expose.requestRoute(rig.conn, specs(), null))) {
        assertEquals(List.of(0, 1, 2), reg.grantedIndexes());
      }
    }
  }

  @Test
  void requestRouteWithoutAnEdgeIsRefusedAndSpecsAreValidated() throws Exception {
    try (Rig rig = new Rig()) {
      // No route policy installed: fail-closed grants nothing or the request is refused; both are
      // "nothing routed".
      try (RouteRegistration reg = await(Expose.requestRoute(rig.conn, specs(), null))) {
        assertEquals(List.of(), reg.grantedIndexes());
      } catch (ExecutionException refused) {
        // a refusal of the whole request
      }
      assertThrows(
          IllegalArgumentException.class,
          () -> new RouteSpec("h", 70_000, RouteSpec.Protocol.HTTP, "s"));
    }
  }

  @Test
  void relayRequestValidatesItsArguments() throws Exception {
    try (Rig rig = new Rig()) {
      assertThrows(
          ExecutionException.class,
          () -> await(Expose.relayRequest(rig.conn, "", "GET", "/", null)));
      assertThrows(
          ExecutionException.class,
          () -> await(Expose.relayRequest(rig.conn, "svc", "", "/", null)));
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
