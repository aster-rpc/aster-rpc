package site.aster.server;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.Map;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import org.apache.fory.Fory;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import site.aster.annotations.Scope;
import site.aster.client.AsterClient;
import site.aster.client.BidiCall;
import site.aster.client.ClientSession;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.contract.CapabilityKind;
import site.aster.contract.CapabilityRequirement;
import site.aster.contract.ContractManifestBuilder;
import site.aster.ffi.Dispatch;
import site.aster.interceptors.CallContext;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;
import site.aster.server.spi.BidiStreamDispatcher;
import site.aster.server.spi.ClientStreamDispatcher;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStream;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ResponseStream;
import site.aster.server.spi.ServerStreamDispatcher;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * Java ↔ Java over dynamic dispatch (C ABI 3.5): every pattern, error mapping, session-scoped
 * services (sessions owned by Rust), Gate 3 ({@code @Requires} enforced in Rust, attributes bridged
 * from {@link AsterServer#setPeerAttributes}), and the JSON lowering's contract id.
 */
final class DispatchServerE2ETest {

  static final String MSG = "_aster_test/DispatchMsg";

  public record Msg(String text, int n) {}

  public static final class Impl {}

  public static final class Counter {
    final AtomicInteger n = new AtomicInteger();
  }

  static MethodDescriptor desc(String name, StreamingKind kind, CapabilityRequirement requires) {
    return new MethodDescriptor(
        name, kind, RequestStyle.EXPLICIT, MSG, List.of(), MSG, false, false, requires);
  }

  static Msg msg(Codec c, byte[] b) {
    return (Msg) c.decode(b, Msg.class);
  }

  static void register(org.apache.fory.BaseFory fory) {
    try {
      fory.register(Msg.class, MSG);
    } catch (Throwable ignored) {
      // already registered
    }
  }

  /** test.Dispatch (SHARED): echo, fail, count, sum, chat, admin. */
  static final class Shared implements ServiceDispatcher {
    @Override
    public ServiceDescriptor descriptor() {
      return new ServiceDescriptor("test.Dispatch", 1, Scope.SHARED, Impl.class);
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      UnaryDispatcher echo =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("echo", StreamingKind.UNARY, null);
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              Msg m = msg(c, req);
              String meta = ctx.metadata().getOrDefault("trace", "");
              return c.encode(new Msg("echo:" + m.text() + meta, m.n()));
            }
          };
      UnaryDispatcher fail =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("fail", StreamingKind.UNARY, null);
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              throw new RpcError(StatusCode.NOT_FOUND, "no such thing", Map.of("why", "test"));
            }
          };
      ServerStreamDispatcher count =
          new ServerStreamDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("count", StreamingKind.SERVER_STREAM, null);
            }

            public void invoke(
                Object impl, byte[] req, Codec c, CallContext ctx, ResponseStream out)
                throws Exception {
              Msg m = msg(c, req);
              for (int i = 0; i < m.n(); i++) {
                out.send(c.encode(new Msg(m.text(), i)));
              }
            }
          };
      ClientStreamDispatcher sum =
          new ClientStreamDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("sum", StreamingKind.CLIENT_STREAM, null);
            }

            public byte[] invoke(Object impl, RequestStream in, Codec c, CallContext ctx)
                throws Exception {
              int total = 0;
              byte[] next;
              while ((next = in.receive()) != null) {
                total += msg(c, next).n();
              }
              return c.encode(new Msg("sum", total));
            }
          };
      BidiStreamDispatcher chat =
          new BidiStreamDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("chat", StreamingKind.BIDI_STREAM, null);
            }

            public void invoke(
                Object impl, RequestStream in, Codec c, CallContext ctx, ResponseStream out)
                throws Exception {
              byte[] next;
              while ((next = in.receive()) != null) {
                out.send(c.encode(new Msg("re:" + msg(c, next).text(), 0)));
              }
            }
          };
      UnaryDispatcher admin =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc(
                  "admin",
                  StreamingKind.UNARY,
                  new CapabilityRequirement(CapabilityKind.ROLE, List.of("admin")));
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              return c.encode(new Msg(ctx.attributes().getOrDefault("aster.role", ""), 0));
            }
          };
      ServerStreamDispatcher silent =
          new ServerStreamDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("silent", StreamingKind.SERVER_STREAM, null);
            }

            public void invoke(
                Object impl, byte[] req, Codec c, CallContext ctx, ResponseStream out)
                throws Exception {
              long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(30);
              while (!out.isCancelled() && System.nanoTime() < end) {
                Thread.sleep(10);
              }
              if (out.isCancelled()) {
                SILENT_CANCELLED.incrementAndGet();
              }
            }
          };
      BidiStreamDispatcher hold =
          new BidiStreamDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("hold", StreamingKind.BIDI_STREAM, null);
            }

            public void invoke(
                Object impl, RequestStream in, Codec c, CallContext ctx, ResponseStream out)
                throws Exception {
              in.receive(); // the first payload arrives with the CALL event
              HOLD_ENTERED.countDown();
              try {
                byte[] next = in.receive(); // blocks until the peer cancels
                HOLD_OUTCOME.complete(next == null ? "end" : "data");
              } catch (java.util.concurrent.CancellationException e) {
                HOLD_OUTCOME.complete("cancelled");
                throw e;
              }
            }
          };
      return Map.of(
          "echo", echo, "fail", fail, "count", count, "sum", sum, "chat", chat, "admin", admin,
          "silent", silent, "hold", hold);
    }

    @Override
    public void registerTypes(Fory fory) {
      register(fory);
    }
  }

  /** test.Counter (SESSION): incr. */
  static final class Session implements ServiceDispatcher {
    @Override
    public ServiceDescriptor descriptor() {
      return new ServiceDescriptor("test.Counter", 1, Scope.SESSION, Counter.class);
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      UnaryDispatcher incr =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("incr", StreamingKind.UNARY, null);
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              return c.encode(new Msg("n", ((Counter) impl).n.incrementAndGet()));
            }
          };
      return Map.of("incr", incr);
    }

    @Override
    public void registerTypes(Fory fory) {
      register(fory);
    }
  }

  /** Count of "silent" handlers that ended because their call was cancelled. */
  static final AtomicInteger SILENT_CANCELLED = new AtomicInteger();

  static final java.util.concurrent.CountDownLatch HOLD_ENTERED =
      new java.util.concurrent.CountDownLatch(1);
  static final java.util.concurrent.CompletableFuture<String> HOLD_OUTCOME =
      new java.util.concurrent.CompletableFuture<>();

  static AsterServer server;
  static AsterClient client;
  static NodeAddr addr;

  @BeforeAll
  static void start() throws Exception {
    ForyCodec serverCodec = new ForyCodec();
    register(serverCodec.fory());
    ForyCodec clientCodec = new ForyCodec();
    register(clientCodec.fory());
    server =
        AsterServer.builder()
            .codec(serverCodec)
            .service(new Impl(), new Shared())
            .sessionService(Counter.class, peer -> new Counter(), new Session())
            .build()
            .get(20, TimeUnit.SECONDS);
    client =
        AsterClient.builder()
            .codec(clientCodec)
            .consumerAdmission(false)
            .build()
            .get(20, TimeUnit.SECONDS);
    addr = server.node().nodeAddr();
  }

  @AfterAll
  static void stop() {
    if (client != null) client.close();
    if (server != null) server.close();
  }

  static <T> T get(java.util.concurrent.CompletableFuture<T> f) throws Exception {
    return f.orTimeout(20, TimeUnit.SECONDS).get();
  }

  static Throwable failure(java.util.concurrent.CompletableFuture<?> f) throws Exception {
    try {
      f.orTimeout(20, TimeUnit.SECONDS).get();
    } catch (ExecutionException e) {
      return e.getCause();
    }
    throw new AssertionError("expected a failure");
  }

  @Test
  void allFourPatternsAndErrors() throws Exception {
    Msg out =
        get(client.<Msg, Msg>call(addr, "test.Dispatch", "echo", new Msg("hi", 1), Msg.class));
    assertEquals("echo:hi", out.text());

    RpcError err =
        assertInstanceOf(
            RpcError.class,
            failure(
                client.<Msg, Msg>call(addr, "test.Dispatch", "fail", new Msg("", 0), Msg.class)));
    assertEquals(StatusCode.NOT_FOUND, err.code());
    assertEquals("no such thing", err.rpcMessage());
    assertEquals("test", err.details().get("why"));

    List<Msg> items =
        get(
            client.<Msg, Msg>callServerStream(
                addr, "test.Dispatch", "count", new Msg("c", 5), Msg.class));
    assertEquals(5, items.size());
    assertEquals(4, items.get(4).n());

    Msg sum =
        get(
            client.<Msg, Msg>callClientStream(
                addr,
                "test.Dispatch",
                "sum",
                List.of(new Msg("a", 1), new Msg("b", 2), new Msg("c", 3)),
                Msg.class));
    assertEquals(6, sum.n());

    try (BidiCall<Msg, Msg> chat =
        get(client.<Msg, Msg>openBidiStream(addr, "test.Dispatch", "chat", Msg.class))) {
      for (String w : List.of("x", "yy", "zzz")) {
        chat.send(new Msg(w, 0));
        assertEquals("re:" + w, chat.recv().text());
      }
      chat.complete();
      assertNull(chat.recv(), "OK trailer");
    }

    // An unknown method is refused in Rust.
    RpcError unknown =
        assertInstanceOf(
            RpcError.class,
            failure(
                client.<Msg, Msg>call(addr, "test.Dispatch", "nope", new Msg("", 0), Msg.class)));
    assertEquals(StatusCode.UNIMPLEMENTED, unknown.code());
    waitForNoLiveCalls();
  }

  @Test
  void sessionsAreOwnedByRust() throws Exception {
    ClientSession a = get(client.openSession(addr));
    ClientSession b = get(client.openSession(addr));
    assertTrue(b.sessionId() > a.sessionId(), "ids from the shared connection counter");
    for (int expect = 1; expect <= 3; expect++) {
      Msg r = get(a.<Msg, Msg>call("test.Counter", "incr", new Msg("", 0), Msg.class));
      assertEquals(expect, r.n());
    }
    assertEquals(1, get(b.<Msg, Msg>call("test.Counter", "incr", new Msg("", 0), Msg.class)).n());
    // A shared service called with a session id: FAILED_PRECONDITION, decided in Rust.
    RpcError e =
        assertInstanceOf(
            RpcError.class,
            failure(a.<Msg, Msg>call("test.Dispatch", "echo", new Msg("", 0), Msg.class)));
    assertEquals(StatusCode.FAILED_PRECONDITION, e.code());
    var snap = server.debugConnectionSnapshot();
    assertEquals(1, snap.size());
    var conn = snap.values().iterator().next();
    assertTrue(conn.activeSessionCount() >= 2);
    assertTrue(conn.lastOpenedSessionId() >= b.sessionId());
    waitForNoLiveCalls();
  }

  @Test
  void requiresIsEnforcedInRustWithBridgedAttributes() throws Exception {
    RpcError denied =
        assertInstanceOf(
            RpcError.class,
            failure(
                client.<Msg, Msg>call(addr, "test.Dispatch", "admin", new Msg("", 0), Msg.class)));
    assertEquals(StatusCode.PERMISSION_DENIED, denied.code());
    server.setPeerAttributes(client.nodeId(), Map.of("aster.role", "admin"));
    try {
      Msg ok =
          get(client.<Msg, Msg>call(addr, "test.Dispatch", "admin", new Msg("", 0), Msg.class));
      assertEquals("admin", ok.text());
    } finally {
      server.clearPeerAttributes(client.nodeId());
    }
    RpcError again =
        assertInstanceOf(
            RpcError.class,
            failure(
                client.<Msg, Msg>call(addr, "test.Dispatch", "admin", new Msg("", 0), Msg.class)));
    assertEquals(StatusCode.PERMISSION_DENIED, again.code());
  }

  @Test
  void jsonLoweringCommitsToTheSameContractId() {
    for (ServiceDispatcher d : List.of(new Shared(), new Session())) {
      ContractManifestBuilder.DynamicServiceSpec spec = ContractManifestBuilder.dynamicService(d);
      assertEquals(spec.contractId(), Dispatch.contractId(spec.json()), spec.json());
      assertEquals(
          ContractManifestBuilder.computeContractId(d), spec.contractId(), "Java's own id");
    }
    assertEquals(
        ContractManifestBuilder.dynamicService(new Shared()).contractId(),
        server.contractId("test.Dispatch"));
  }

  /**
   * A handler blocked in {@code Dispatch.recv} (bounded native waits, no infinite timeout) is
   * released when the peer cancels: the request stream ends ({@code null}, the call is over) or
   * {@code receive} throws {@link java.util.concurrent.CancellationException}.
   */
  @Test
  void handlerBlockedInRecvIsReleasedByPeerCancel() throws Exception {
    try (BidiCall<Msg, Msg> call =
        get(client.<Msg, Msg>openBidiStream(addr, "test.Dispatch", "hold", Msg.class))) {
      call.send(new Msg("first", 0));
      assertTrue(HOLD_ENTERED.await(10, TimeUnit.SECONDS), "handler reached its second receive");
      Thread.sleep(300); // let it sit in more than one recv slice
      call.cancel();
      String outcome = HOLD_OUTCOME.get(5, TimeUnit.SECONDS);
      assertTrue(outcome.equals("cancelled") || outcome.equals("end"), outcome);
      RpcError e =
          assertInstanceOf(
              RpcError.class,
              org.junit.jupiter.api.Assertions.assertThrows(RpcError.class, call::recv));
      assertEquals(StatusCode.CANCELLED, e.code());
    }
    waitForNoLiveCalls();
  }

  /** Closing an open-ended server stream stops the reader's bounded-wait loop promptly. */
  @Test
  void closingAWaitingServerStreamStopsTheReader() throws Exception {
    int cancelledBefore = SILENT_CANCELLED.get();
    var call =
        get(
            client.<Msg, Msg>openServerStream(
                addr, "test.Dispatch", "silent", new Msg("", 0), Msg.class));
    assertNull(call.recvWithTimeout(300, TimeUnit.MILLISECONDS), "nothing arrives");
    long t0 = System.nanoTime();
    call.close();
    assertTrue(
        System.nanoTime() - t0 < TimeUnit.SECONDS.toNanos(3), "close does not wait on the reader");
    long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
    while (SILENT_CANCELLED.get() == cancelledBefore && System.nanoTime() < end) {
      Thread.sleep(10);
    }
    assertTrue(SILENT_CANCELLED.get() > cancelledBefore, "the handler saw the cancel");
    waitForNoLiveCalls();
  }

  /**
   * {@link site.aster.ffi.Rpc#recvPolling} observes its own cancellation between bounded waits: an
   * interrupted reader, or one whose stop condition turns true, cancels the native call and returns
   * the {@code CANCELLED} trailer.
   */
  @Test
  void recvPollingObservesInterruptAndStop() throws Exception {
    var conn = get(client.connect(addr));
    long rt = conn.runtime().nativeHandle();
    for (boolean viaInterrupt : new boolean[] {true, false}) {
      long call =
          site.aster.ffi.Rpc.open(
              rt,
              conn.nativeHandle(),
              site.aster.ffi.Rpc.PATTERN_SERVER_STREAM,
              site.aster.ffi.Rpc.Options.of("test.Dispatch", "silent", 0));
      try {
        site.aster.ffi.Rpc.send(rt, call, client.codec().encode(new Msg("", 0)), true);
        final long c = call;
        var stop = new java.util.concurrent.atomic.AtomicBoolean();
        var result = new java.util.concurrent.CompletableFuture<site.aster.ffi.Rpc.Recv>();
        Thread reader =
            new Thread(
                () -> {
                  try {
                    result.complete(site.aster.ffi.Rpc.recvPolling(rt, c, stop::get));
                  } catch (Throwable t) {
                    result.completeExceptionally(t);
                  }
                });
        reader.start();
        Thread.sleep(300);
        assertTrue(!result.isDone(), "still waiting");
        if (viaInterrupt) {
          reader.interrupt();
        } else {
          stop.set(true);
        }
        var r = result.get(5, TimeUnit.SECONDS);
        var trailer = assertInstanceOf(site.aster.ffi.Rpc.Recv.Trailer.class, r);
        assertEquals(
            site.aster.server.wire.RpcStatus.CANCELLED,
            site.aster.ffi.AsterEnvelope.decodeRpcStatus(trailer.status()).code(),
            "via interrupt=" + viaInterrupt);
      } finally {
        site.aster.ffi.Rpc.release(rt, call);
      }
    }
    waitForNoLiveCalls();
  }

  static void waitForNoLiveCalls() throws InterruptedException {
    long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
    while (server.liveCalls() != 0 && System.nanoTime() < end) {
      Thread.sleep(10);
    }
    assertEquals(0, server.liveCalls(), "Rust's call table drains");
  }
}
