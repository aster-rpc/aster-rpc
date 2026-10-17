package site.aster.server;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.BooleanSupplier;
import org.apache.fory.Fory;
import org.junit.jupiter.api.Test;
import site.aster.annotations.Scope;
import site.aster.client.AsterClient;
import site.aster.client.CallOptions;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;
import site.aster.interceptors.CallContext;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * ABI 7.2 server drain and the ABI 7.5 handler context (transport peer vs principal, call ids,
 * metadata, deadline, {@code current()}, {@code cancelled()}), over Iroh with a typed client.
 */
final class DrainAndCallContextE2ETest {

  static final String MSG = "_aster_test/DrainMsg";

  public record Msg(String text, int n) {}

  public static final class Impl {}

  static MethodDescriptor desc(String name) {
    return new MethodDescriptor(
        name, StreamingKind.UNARY, RequestStyle.EXPLICIT, MSG, List.of(), MSG, false, false, null);
  }

  /** test.Drain: hold (waits for release or cancellation), ping, whoami. */
  static final class Svc implements ServiceDispatcher {
    final AtomicInteger started = new AtomicInteger();
    final AtomicInteger cancelled = new AtomicInteger();
    final CompletableFuture<Void> release = new CompletableFuture<>();
    final AtomicReference<CallContext> seen = new AtomicReference<>();
    final AtomicReference<CallContext> current = new AtomicReference<>();

    @Override
    public ServiceDescriptor descriptor() {
      return new ServiceDescriptor("test.Drain", 1, Scope.SHARED, Impl.class);
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      UnaryDispatcher hold =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("hold");
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx)
                throws Exception {
              started.incrementAndGet();
              CompletableFuture<Object> either = CompletableFuture.anyOf(release, ctx.cancelled());
              either.get(60, TimeUnit.SECONDS);
              if (ctx.isCancelled() && !release.isDone()) {
                cancelled.incrementAndGet();
                throw new RpcError(StatusCode.CANCELLED, "cancelled");
              }
              return c.encode(new Msg("held", 0));
            }
          };
      UnaryDispatcher ping =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("ping");
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              return c.encode(new Msg("pong", 0));
            }
          };
      UnaryDispatcher whoami =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("whoami");
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              seen.set(ctx);
              current.set(CallContext.currentOrNull());
              return c.encode(new Msg(ctx.peer(), 0));
            }
          };
      return Map.of("hold", hold, "ping", ping, "whoami", whoami);
    }

    @Override
    public void registerTypes(Fory fory) {
      register(fory);
    }
  }

  static void register(org.apache.fory.BaseFory fory) {
    try {
      fory.register(Msg.class, MSG);
    } catch (Throwable ignored) {
      // swallow-ok: already registered by an earlier test in this process
    }
  }

  record Fixture(AsterServer server, AsterClient client, NodeAddr addr, Svc svc)
      implements AutoCloseable {
    static Fixture start(boolean health) throws Exception {
      ForyCodec serverCodec = new ForyCodec();
      register(serverCodec.fory());
      ForyCodec clientCodec = new ForyCodec();
      register(clientCodec.fory());
      Svc svc = new Svc();
      AsterServer.Builder b = AsterServer.builder().codec(serverCodec).service(new Impl(), svc);
      if (health) {
        b.health("127.0.0.1", 0);
      }
      AsterServer server = b.build().get(20, TimeUnit.SECONDS);
      AsterClient client =
          AsterClient.builder().codec(clientCodec).build().get(20, TimeUnit.SECONDS);
      return new Fixture(server, client, server.node().nodeAddr(), svc);
    }

    CompletableFuture<Msg> call(String method, CallOptions options) {
      return client.call(addr, "test.Drain", method, new Msg("x", 0), Msg.class, options);
    }

    @Override
    public void close() {
      client.close();
      server.close();
    }
  }

  static void eventually(String what, BooleanSupplier cond) throws InterruptedException {
    long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
    while (System.nanoTime() < end) {
      if (cond.getAsBoolean()) return;
      Thread.sleep(25);
    }
    throw new AssertionError("timed out waiting for " + what);
  }

  static int readyz(HttpClient http, String address) {
    try {
      return http.send(
              HttpRequest.newBuilder(URI.create("http://" + address + "/readyz")).build(),
              HttpResponse.BodyHandlers.discarding())
          .statusCode();
    } catch (Exception e) {
      return -1;
    }
  }

  @Test
  void handlerContextCarriesIdentityIdsMetadataAndDeadline() throws Exception {
    try (Fixture f = Fixture.start(false)) {
      Msg out =
          f.call(
                  "whoami",
                  CallOptions.DEFAULT
                      .withTimeout(Duration.ofMillis(30_500))
                      .withMetadata("tenant", "t-1"))
              .get(20, TimeUnit.SECONDS);
      CallContext ctx = f.svc().seen.get();
      assertNotNull(ctx);
      String clientId = f.client().nodeId();
      // Over Iroh with no Authenticator the transport peer is also the principal.
      assertEquals(clientId, ctx.peer());
      assertEquals(clientId, ctx.principal());
      assertEquals(clientId, out.text());
      assertTrue(ctx.irohAuthenticated());
      assertSame(ctx, f.svc().current.get(), "CallContext.current() inside the handler");
      assertTrue(ctx.dispatchCallId() > 0, "Rust's call id");
      assertEquals(Integer.toString(ctx.wireCallId()), ctx.callId());
      assertEquals("t-1", ctx.metadataValue("tenant"));
      Duration remaining = ctx.remaining();
      assertNotNull(remaining);
      assertTrue(remaining.compareTo(Duration.ofSeconds(31)) <= 0, remaining.toString());
      assertNotNull(ctx.deadlineInstant());
      assertFalse(ctx.cancelled().isDone());

      f.call("whoami", CallOptions.DEFAULT).get(20, TimeUnit.SECONDS);
      CallContext plain = f.svc().seen.get();
      // Without a caller deadline the effective one is the server's handler timeout, later than
      // the 30.5 s the first call asked for.
      if (plain.deadlineInstant() != null) {
        assertTrue(
            plain.remaining().compareTo(Duration.ofSeconds(31)) > 0, plain.remaining().toString());
      }
      assertTrue(plain.dispatchCallId() > ctx.dispatchCallId());
    }
    assertNull(CallContext.currentOrNull());
  }

  @Test
  void drainLetsInFlightFinishRefusesNewCallsAndTurnsReadinessOff() throws Exception {
    try (Fixture f = Fixture.start(true);
        HttpClient http = HttpClient.newHttpClient()) {
      String health = f.server().health().address();
      assertEquals(200, readyz(http, health));
      assertFalse(f.server().isDraining());
      CompletableFuture<Msg> held = f.call("hold", CallOptions.DEFAULT);
      eventually("the held call in its handler", () -> f.svc().started.get() == 1);

      CompletableFuture<DrainReport> drained =
          CompletableFuture.supplyAsync(() -> f.server().drain(Duration.ofSeconds(30)));
      AtomicReference<Throwable> refused = new AtomicReference<>();
      eventually(
          "a new call refused as draining",
          () -> {
            try {
              f.call("ping", CallOptions.DEFAULT).get(10, TimeUnit.SECONDS);
              return false;
            } catch (Exception e) {
              refused.set(e);
              return true;
            }
          });
      assertTrue(f.server().isDraining());
      assertTrue(RpcError.isDraining(refused.get()), String.valueOf(refused.get()));
      RpcError e = assertInstanceOf(RpcError.class, refused.get().getCause());
      assertEquals(StatusCode.UNAVAILABLE, e.code());
      assertTrue(e.isDraining());
      eventually("readiness off", () -> readyz(http, health) != 200);
      assertFalse(drained.isDone(), "drain waits for the held call");

      f.svc().release.complete(null);
      assertEquals("held", held.get(20, TimeUnit.SECONDS).text());
      DrainReport r = drained.get(30, TimeUnit.SECONDS);
      assertTrue(r.callsInFlight() >= 1, r.toString());
      assertEquals(r.callsInFlight(), r.callsCompleted(), r.toString());
      assertEquals(0, r.callsCancelled(), r.toString());
      assertTrue(r.callsRefused() >= 1, r.toString());
      assertEquals(1, r.connectionsClosed(), r.toString());
      assertFalse(r.deadlineReached());
      assertTrue(r.elapsed().compareTo(Duration.ofSeconds(30)) < 0);
      assertEquals(0, f.svc().cancelled.get());

      IrohException closed =
          assertThrows(IrohException.class, () -> f.server().drain(Duration.ZERO));
      assertEquals(IrohStatus.ALREADY_CLOSED, closed.status());
    }
  }

  @Test
  void callRunningAtTheDeadlineIsCancelledAndSeesCancelled() throws Exception {
    try (Fixture f = Fixture.start(false)) {
      CompletableFuture<Msg> held = f.call("hold", CallOptions.DEFAULT);
      eventually("the held call in its handler", () -> f.svc().started.get() == 1);
      DrainReport r = f.server().drain(Duration.ofMillis(200));
      assertTrue(r.deadlineReached(), r.toString());
      assertEquals(1, r.callsCancelled(), r.toString());
      eventually("the handler observed cancelled()", () -> f.svc().cancelled.get() == 1);
      ExecutionException failure =
          assertThrows(ExecutionException.class, () -> held.get(20, TimeUnit.SECONDS));
      assertNotNull(failure.getCause());
      assertThrows(IllegalArgumentException.class, () -> f.server().drain(Duration.ofSeconds(-1)));
    }
  }
}
