package site.aster.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.time.Duration;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CancellationException;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import site.aster.annotations.Scope;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.interceptors.CallContext;
import site.aster.interceptors.CircuitBreakerInterceptor;
import site.aster.interceptors.DeadlineInterceptor;
import site.aster.interceptors.Interceptor;
import site.aster.interceptors.RetryInterceptor;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;
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
 * Client interceptors (deadline, retry, circuit breaker, user interceptors) on every call pattern,
 * and native unary cancel, against a real {@link AsterServer} over loopback: ordering, retry
 * semantics (idempotent only, bounded), breaker state, deadline propagation, and that cancelling a
 * cancellable call reaches the server handler.
 */
final class ClientInterceptorsE2ETest {

  static final String MSG = "_aster_test/IceptMsg";
  static final String SVC = "test.Icept";
  static final String SESSION_SVC = "test.IceptSession";

  public record Msg(String text, int n) {}

  public static final class Impl {}

  /** Calls seen per {@code text} key by the flaky / boom methods. */
  static final Map<String, AtomicInteger> CALLS = new ConcurrentHashMap<>();

  /** Handlers of {@code slow} that observed their call being cancelled. */
  static final AtomicInteger SLOW_CANCELLED = new AtomicInteger();

  static volatile CountDownLatch slowEntered = new CountDownLatch(1);
  static volatile CountDownLatch slowCancelled = new CountDownLatch(1);

  static AtomicInteger calls(String key) {
    return CALLS.computeIfAbsent(key, k -> new AtomicInteger());
  }

  static MethodDescriptor desc(String name, StreamingKind kind) {
    return new MethodDescriptor(
        name, kind, RequestStyle.EXPLICIT, MSG, List.of(), MSG, false, false);
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

  static final class Svc implements ServiceDispatcher {
    @Override
    public ServiceDescriptor descriptor() {
      return new ServiceDescriptor(SVC, 1, Scope.SHARED, Impl.class);
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      // echo: "echo:<text>|<trace metadata>"
      UnaryDispatcher echo =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("echo", StreamingKind.UNARY);
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              Msg m = msg(c, req);
              return c.encode(
                  new Msg("echo:" + m.text() + "|" + ctx.metadata().getOrDefault("trace", ""), 0));
            }
          };
      // flaky: UNAVAILABLE until the n-th call for this text, then succeeds with the call count
      UnaryDispatcher flaky =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("flaky", StreamingKind.UNARY);
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              Msg m = msg(c, req);
              int seen = calls(m.text()).incrementAndGet();
              if (seen < m.n()) {
                throw new RpcError(StatusCode.UNAVAILABLE, "try again " + seen);
              }
              return c.encode(new Msg("ok", seen));
            }
          };
      UnaryDispatcher boom =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("boom", StreamingKind.UNARY);
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              Msg m = msg(c, req);
              calls(m.text()).incrementAndGet();
              throw new RpcError(StatusCode.INTERNAL, "boom");
            }
          };
      // deadline: whether the server saw a deadline, and its remaining whole seconds
      UnaryDispatcher deadline =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("deadline", StreamingKind.UNARY);
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              Double rem = ctx.remainingSeconds();
              return c.encode(
                  new Msg(rem == null ? "none" : "some", rem == null ? 0 : (int) Math.ceil(rem)));
            }
          };
      // slow: runs until its call is cancelled (or 30 s); reports the cancel
      UnaryDispatcher slow =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("slow", StreamingKind.UNARY);
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx)
                throws Exception {
              slowEntered.countDown();
              long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(30);
              while (!ctx.isCancelled() && System.nanoTime() < end) {
                Thread.sleep(5);
              }
              if (ctx.isCancelled()) {
                SLOW_CANCELLED.incrementAndGet();
                slowCancelled.countDown();
              }
              return c.encode(new Msg("slow-done", 0));
            }
          };
      ServerStreamDispatcher count =
          new ServerStreamDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("count", StreamingKind.SERVER_STREAM);
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
      ServerStreamDispatcher streamFail =
          new ServerStreamDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("streamFail", StreamingKind.SERVER_STREAM);
            }

            public void invoke(
                Object impl, byte[] req, Codec c, CallContext ctx, ResponseStream out)
                throws Exception {
              out.send(c.encode(new Msg("first", 0)));
              throw new RpcError(StatusCode.NOT_FOUND, "stream gone");
            }
          };
      // sum: client stream; the n-th call (per first text) succeeds, earlier ones are UNAVAILABLE
      ClientStreamDispatcher sum =
          new ClientStreamDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("sum", StreamingKind.CLIENT_STREAM);
            }

            public byte[] invoke(Object impl, RequestStream in, Codec c, CallContext ctx)
                throws Exception {
              int total = 0;
              int need = 1;
              String key = "";
              byte[] next;
              while ((next = in.receive()) != null) {
                Msg m = msg(c, next);
                if (key.isEmpty()) {
                  key = m.text();
                  need = Math.max(1, Integer.parseInt(ctx.metadata().getOrDefault("need", "1")));
                }
                total += m.n();
              }
              if (calls(key).incrementAndGet() < need) {
                throw new RpcError(StatusCode.UNAVAILABLE, "sum not yet");
              }
              return c.encode(new Msg("sum", total));
            }
          };
      BidiStreamDispatcher chat =
          new BidiStreamDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("chat", StreamingKind.BIDI_STREAM);
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
      return Map.of(
          "echo",
          echo,
          "flaky",
          flaky,
          "boom",
          boom,
          "deadline",
          deadline,
          "slow",
          slow,
          "count",
          count,
          "streamFail",
          streamFail,
          "sum",
          sum,
          "chat",
          chat);
    }

    @Override
    public void registerTypes(org.apache.fory.Fory fory) {
      register(fory);
    }
  }

  public static final class Counter {
    final AtomicInteger n = new AtomicInteger();
  }

  /** test.IceptSession (SESSION): incr. */
  static final class Session implements ServiceDispatcher {
    @Override
    public ServiceDescriptor descriptor() {
      return new ServiceDescriptor(SESSION_SVC, 1, Scope.SESSION, Counter.class);
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      UnaryDispatcher incr =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return desc("incr", StreamingKind.UNARY);
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              return c.encode(new Msg("n", ((Counter) impl).n.incrementAndGet()));
            }
          };
      return Map.of("incr", incr);
    }

    @Override
    public void registerTypes(org.apache.fory.Fory fory) {
      register(fory);
    }
  }

  static AsterServer server;
  static NodeAddr addr;
  static final List<AsterClient> clients = Collections.synchronizedList(new ArrayList<>());

  @BeforeAll
  static void start() throws Exception {
    ForyCodec serverCodec = new ForyCodec();
    register(serverCodec.fory());
    server =
        AsterServer.builder()
            .codec(serverCodec)
            .service(new Impl(), new Svc())
            .sessionService(Counter.class, peer -> new Counter(), new Session())
            .build()
            .get(20, TimeUnit.SECONDS);
    addr = server.node().nodeAddr();
  }

  @AfterAll
  static void stop() {
    clients.forEach(AsterClient::close);
    if (server != null) server.close();
  }

  @BeforeEach
  void reset() {
    CALLS.clear();
    SLOW_CANCELLED.set(0);
    slowEntered = new CountDownLatch(1);
    slowCancelled = new CountDownLatch(1);
  }

  static AsterClient client(Interceptor... interceptors) throws Exception {
    ForyCodec codec = new ForyCodec();
    register(codec.fory());
    AsterClient c =
        AsterClient.builder()
            .codec(codec)
            .consumerAdmission(false)
            .interceptors(List.of(interceptors))
            .build()
            .get(20, TimeUnit.SECONDS);
    clients.add(c);
    return c;
  }

  static <T> T get(CompletableFuture<T> f) throws Exception {
    return f.orTimeout(20, TimeUnit.SECONDS).get();
  }

  static Throwable failure(CompletableFuture<?> f) throws Exception {
    try {
      f.orTimeout(20, TimeUnit.SECONDS).get();
    } catch (ExecutionException e) {
      return e.getCause();
    }
    throw new AssertionError("expected a failure");
  }

  /** Records each hook, tagged with the interceptor's name, and optionally transforms. */
  static final class Recorder implements Interceptor {
    final String name;
    final List<String> log;

    Recorder(String name, List<String> log) {
      this.name = name;
      this.log = log;
    }

    @Override
    public Object onRequest(CallContext ctx, Object request) {
      log.add(name + ":req:" + ctx.pattern());
      return request;
    }

    @Override
    public Object onResponse(CallContext ctx, Object response) {
      log.add(name + ":resp");
      return response;
    }

    @Override
    public RpcError onError(CallContext ctx, RpcError error) {
      log.add(name + ":err:" + error.code());
      return error;
    }
  }

  // ───── ordering, every pattern ──────────────────────────────────────────

  @Test
  void unaryOrderingForwardThenReverseOnError() throws Exception {
    List<String> log = Collections.synchronizedList(new ArrayList<>());
    AsterClient c = client(new Recorder("A", log), new Recorder("B", log));
    get(c.<Msg, Msg>call(addr, SVC, "echo", new Msg("x", 0), Msg.class));
    assertEquals(List.of("A:req:unary", "B:req:unary", "A:resp", "B:resp"), log);

    log.clear();
    Throwable t = failure(c.<Msg, Msg>call(addr, SVC, "boom", new Msg("ord", 0), Msg.class));
    assertEquals(StatusCode.INTERNAL, assertInstanceOf(RpcError.class, t).code());
    assertEquals(List.of("A:req:unary", "B:req:unary", "B:err:INTERNAL", "A:err:INTERNAL"), log);
  }

  @Test
  void serverStreamRunsRequestOnceAndResponseOnEveryItem() throws Exception {
    List<String> log = Collections.synchronizedList(new ArrayList<>());
    AsterClient c = client(new Recorder("A", log));
    List<Msg> items =
        get(c.<Msg, Msg>callServerStream(addr, SVC, "count", new Msg("c", 3), Msg.class));
    assertEquals(3, items.size());
    assertEquals(List.of("A:req:server_stream", "A:resp", "A:resp", "A:resp"), log);

    log.clear();
    Throwable t =
        failure(c.<Msg, Msg>callServerStream(addr, SVC, "streamFail", new Msg("", 0), Msg.class));
    assertEquals(StatusCode.NOT_FOUND, ((RpcError) t).code());
    assertEquals(List.of("A:req:server_stream", "A:resp", "A:err:NOT_FOUND"), log);
  }

  @Test
  void incrementalServerStreamIsIntercepted() throws Exception {
    List<String> log = Collections.synchronizedList(new ArrayList<>());
    AsterClient c = client(new Recorder("A", log));
    try (ServerStreamCall<Msg> call =
        get(c.<Msg, Msg>openServerStream(addr, SVC, "count", new Msg("c", 2), Msg.class))) {
      assertEquals(0, call.recv().n());
      assertEquals(1, call.recv().n());
      assertNull(call.recv());
    }
    assertEquals(List.of("A:req:server_stream", "A:resp", "A:resp"), log);
  }

  @Test
  void clientStreamRunsRequestOnEveryItem() throws Exception {
    List<String> log = Collections.synchronizedList(new ArrayList<>());
    AsterClient c = client(new Recorder("A", log));
    Msg out =
        get(
            c.<Msg, Msg>callClientStream(
                addr, SVC, "sum", List.of(new Msg("cs-order", 1), new Msg("b", 2)), Msg.class));
    assertEquals(3, out.n());
    assertEquals(List.of("A:req:client_stream", "A:req:client_stream", "A:resp"), log);
  }

  @Test
  void bidiBufferedAndInteractiveAreIntercepted() throws Exception {
    List<String> log = Collections.synchronizedList(new ArrayList<>());
    AsterClient c = client(new Recorder("A", log));
    List<Msg> out =
        get(
            c.<Msg, Msg>callBidiStream(
                addr, SVC, "chat", List.of(new Msg("a", 0), new Msg("b", 0)), Msg.class));
    assertEquals(List.of("re:a", "re:b"), out.stream().map(Msg::text).toList());
    assertEquals(List.of("A:req:bidi_stream", "A:req:bidi_stream", "A:resp", "A:resp"), log);

    log.clear();
    try (BidiCall<Msg, Msg> chat = get(c.<Msg, Msg>openBidiStream(addr, SVC, "chat", Msg.class))) {
      chat.send(new Msg("x", 0));
      assertEquals("re:x", chat.recv().text());
      chat.complete();
      assertNull(chat.recv());
    }
    assertEquals(List.of("A:req:bidi_stream", "A:resp"), log);
  }

  @Test
  void sessionCallsAreIntercepted() throws Exception {
    List<String> log = Collections.synchronizedList(new ArrayList<>());
    AsterClient c = client(new Recorder("A", log));
    try (ClientSession s = get(c.openSession(addr))) {
      assertEquals(1, get(s.<Msg, Msg>call(SESSION_SVC, "incr", new Msg("s", 0), Msg.class)).n());
      assertEquals(2, get(s.<Msg, Msg>call(SESSION_SVC, "incr", new Msg("s", 0), Msg.class)).n());
    }
    assertEquals(List.of("A:req:unary", "A:resp", "A:req:unary", "A:resp"), log);
  }

  @Test
  void userInterceptorsTransformAndInjectMetadata() throws Exception {
    Interceptor stamp =
        new Interceptor() {
          @Override
          public Object onRequest(CallContext ctx, Object request) {
            ctx.metadata().put("trace", "t1");
            Msg m = (Msg) request;
            return new Msg(m.text() + "!", m.n());
          }

          @Override
          public Object onResponse(CallContext ctx, Object response) {
            Msg m = (Msg) response;
            return new Msg(m.text().toUpperCase(), m.n());
          }
        };
    AsterClient c = client(stamp);
    Msg out = get(c.<Msg, Msg>call(addr, SVC, "echo", new Msg("hi", 0), Msg.class));
    assertEquals("ECHO:HI!|T1", out.text(), "request rewritten, metadata sent, response rewritten");
  }

  @Test
  void anErrorInterceptorReturningNullSuppressesTheError() throws Exception {
    Interceptor swallow =
        new Interceptor() {
          @Override
          public RpcError onError(CallContext ctx, RpcError error) {
            return null;
          }
        };
    AsterClient c = client(swallow);
    assertNull(get(c.<Msg, Msg>call(addr, SVC, "boom", new Msg("sup", 0), Msg.class)));
  }

  // ───── retry ────────────────────────────────────────────────────────────

  private static RetryInterceptor retry(int attempts) {
    return new RetryInterceptor(attempts, Set.of(StatusCode.UNAVAILABLE), 5, 20, 2.0, 0.0);
  }

  @Test
  void idempotentUnaryIsRetriedUntilItSucceeds() throws Exception {
    AsterClient c = client(retry(5));
    Msg out =
        get(
            c.<Msg, Msg>call(
                addr,
                SVC,
                "flaky",
                new Msg("r1", 3),
                Msg.class,
                CallOptions.DEFAULT.withIdempotent(true)));
    assertEquals(3, out.n());
    assertEquals(3, calls("r1").get());
  }

  @Test
  void nonIdempotentCallsAreNeverRetried() throws Exception {
    AsterClient c = client(retry(5));
    Throwable t = failure(c.<Msg, Msg>call(addr, SVC, "flaky", new Msg("r2", 3), Msg.class));
    assertEquals(StatusCode.UNAVAILABLE, ((RpcError) t).code());
    assertEquals(1, calls("r2").get());
  }

  @Test
  void retriesAreBoundedByMaxAttempts() throws Exception {
    AsterClient c = client(retry(3));
    Throwable t =
        failure(
            c.<Msg, Msg>call(
                addr,
                SVC,
                "flaky",
                new Msg("r3", 99),
                Msg.class,
                CallOptions.DEFAULT.withIdempotent(true)));
    assertEquals(StatusCode.UNAVAILABLE, ((RpcError) t).code());
    assertEquals(3, calls("r3").get());
  }

  @Test
  void nonRetryableCodesAreNotRetried() throws Exception {
    AsterClient c = client(retry(5));
    Throwable t =
        failure(
            c.<Msg, Msg>call(
                addr,
                SVC,
                "boom",
                new Msg("r4", 0),
                Msg.class,
                CallOptions.DEFAULT.withIdempotent(true)));
    assertEquals(StatusCode.INTERNAL, ((RpcError) t).code());
    assertEquals(1, calls("r4").get());
  }

  @Test
  void clientStreamingIsRetriedAndResendsEveryRequest() throws Exception {
    AsterClient c = client(retry(4));
    Msg out =
        get(
            c.<Msg, Msg>callClientStream(
                addr,
                SVC,
                "sum",
                List.of(new Msg("r5", 1), new Msg("b", 2), new Msg("c", 3)),
                Msg.class,
                CallOptions.DEFAULT.withIdempotent(true).withMetadata("need", "3")));
    assertEquals(6, out.n(), "the final attempt carried all the requests");
    assertEquals(3, calls("r5").get());
  }

  @Test
  void streamingCallsWithHandlesAreNotRetried() throws Exception {
    // server-stream is not retried (Python parity): a failing stream fails once
    AsterClient c = client(retry(5));
    Throwable t =
        failure(
            c.<Msg, Msg>callServerStream(
                addr,
                SVC,
                "streamFail",
                new Msg("", 0),
                Msg.class,
                CallOptions.DEFAULT.withIdempotent(true)));
    assertEquals(StatusCode.NOT_FOUND, ((RpcError) t).code());
  }

  @Test
  void attemptNumberAndErrorChainRunOncePerAttempt() throws Exception {
    List<String> log = Collections.synchronizedList(new ArrayList<>());
    Interceptor seen =
        new Interceptor() {
          @Override
          public Object onRequest(CallContext ctx, Object request) {
            log.add("attempt" + ctx.attempt());
            return request;
          }
        };
    AsterClient c = client(retry(5), seen);
    get(
        c.<Msg, Msg>call(
            addr,
            SVC,
            "flaky",
            new Msg("r6", 3),
            Msg.class,
            CallOptions.DEFAULT.withIdempotent(true)));
    assertEquals(List.of("attempt1", "attempt2", "attempt3"), log);
  }

  // ───── circuit breaker ──────────────────────────────────────────────────

  @Test
  void breakerOpensAfterConsecutiveFailuresThenRecovers() throws Exception {
    CircuitBreakerInterceptor breaker = new CircuitBreakerInterceptor(3, 0.4, 1);
    AsterClient c = client(breaker);
    for (int i = 0; i < 3; i++) {
      Throwable t = failure(c.<Msg, Msg>call(addr, SVC, "boom", new Msg("b1", 0), Msg.class));
      assertEquals(StatusCode.INTERNAL, ((RpcError) t).code());
    }
    assertEquals(CircuitBreakerInterceptor.State.OPEN, breaker.state());
    assertEquals(3, calls("b1").get());

    // open: rejected locally, the server never sees it
    Throwable open = failure(c.<Msg, Msg>call(addr, SVC, "boom", new Msg("b1", 0), Msg.class));
    assertEquals(StatusCode.UNAVAILABLE, ((RpcError) open).code());
    assertTrue(((RpcError) open).rpcMessage().contains("circuit breaker is open"));
    assertEquals(3, calls("b1").get());
    assertEquals(
        CircuitBreakerInterceptor.State.OPEN, breaker.state(), "a rejection is not a new failure");

    // after the recovery timeout one probe is let through; success closes the circuit
    Thread.sleep(500);
    get(c.<Msg, Msg>call(addr, SVC, "echo", new Msg("probe", 0), Msg.class));
    assertEquals(CircuitBreakerInterceptor.State.CLOSED, breaker.state());
  }

  @Test
  void aFailingHalfOpenProbeReopensTheBreaker() throws Exception {
    CircuitBreakerInterceptor breaker = new CircuitBreakerInterceptor(1, 0.3, 1);
    AsterClient c = client(breaker);
    failure(c.<Msg, Msg>call(addr, SVC, "boom", new Msg("b2", 0), Msg.class));
    assertEquals(CircuitBreakerInterceptor.State.OPEN, breaker.state());
    Thread.sleep(400);
    failure(c.<Msg, Msg>call(addr, SVC, "boom", new Msg("b2", 0), Msg.class)); // the probe
    assertEquals(2, calls("b2").get());
    assertEquals(CircuitBreakerInterceptor.State.OPEN, breaker.state());
  }

  @Test
  void breakerCountsEachRetryAttempt() throws Exception {
    // threshold 2, retry 5: the second UNAVAILABLE opens the circuit and the third attempt is
    // rejected locally (not retried further, the server saw two calls)
    CircuitBreakerInterceptor breaker = new CircuitBreakerInterceptor(2, 30.0, 1);
    AsterClient c = client(retry(5), breaker);
    Throwable t =
        failure(
            c.<Msg, Msg>call(
                addr,
                SVC,
                "flaky",
                new Msg("b3", 99),
                Msg.class,
                CallOptions.DEFAULT.withIdempotent(true)));
    assertTrue(((RpcError) t).rpcMessage().contains("circuit breaker is open"));
    assertEquals(2, calls("b3").get());
  }

  // ───── deadline ─────────────────────────────────────────────────────────

  @Test
  void timeoutIsPropagatedToTheServerAsTheRemainingDeadline() throws Exception {
    AsterClient c = client(new DeadlineInterceptor());
    // without a timeout the server still applies its own handler cap, which is larger
    Msg uncapped = get(c.<Msg, Msg>call(addr, SVC, "deadline", new Msg("", 0), Msg.class));
    assertTrue(uncapped.text().equals("none") || uncapped.n() > 5, "cap " + uncapped.n());
    Msg some =
        get(
            c.<Msg, Msg>call(
                addr,
                SVC,
                "deadline",
                new Msg("", 0),
                Msg.class,
                CallOptions.DEFAULT.withTimeout(Duration.ofSeconds(5))));
    assertEquals("some", some.text());
    assertTrue(some.n() >= 1 && some.n() <= 5, "remaining " + some.n());
  }

  @Test
  void timeoutAppliesWithoutInterceptorsToo() throws Exception {
    AsterClient c = client();
    Msg some =
        get(
            c.<Msg, Msg>call(
                addr,
                SVC,
                "deadline",
                new Msg("", 0),
                Msg.class,
                CallOptions.DEFAULT.withTimeout(Duration.ofSeconds(30))));
    assertEquals("some", some.text());
  }

  @Test
  void anExpiredDeadlineEndsTheCallAndReachesTheHandler() throws Exception {
    AsterClient c = client(new DeadlineInterceptor());
    long t0 = System.nanoTime();
    Throwable t =
        failure(
            c.<Msg, Msg>call(
                addr,
                SVC,
                "slow",
                new Msg("", 0),
                Msg.class,
                CallOptions.DEFAULT.withTimeout(Duration.ofSeconds(1))));
    assertEquals(StatusCode.DEADLINE_EXCEEDED, ((RpcError) t).code());
    assertTrue(
        Duration.ofNanos(System.nanoTime() - t0).toSeconds() < 8,
        "the deadline, not the handler's 30 s");
    assertTrue(slowCancelled.await(10, TimeUnit.SECONDS), "the handler saw its call end");
  }

  @Test
  void retryBackoffNeverOutlivesTheDeadline() throws Exception {
    // every attempt fails UNAVAILABLE; a 300 ms deadline must stop the retries
    RetryInterceptor slowRetry =
        new RetryInterceptor(50, Set.of(StatusCode.UNAVAILABLE), 200, 200, 1.0, 0.0);
    AsterClient c = client(slowRetry, new DeadlineInterceptor());
    long t0 = System.nanoTime();
    Throwable t =
        failure(
            c.<Msg, Msg>call(
                addr,
                SVC,
                "flaky",
                new Msg("d1", 99),
                Msg.class,
                CallOptions.DEFAULT.withIdempotent(true).withTimeout(Duration.ofMillis(300))));
    assertInstanceOf(RpcError.class, t);
    assertTrue(
        Duration.ofNanos(System.nanoTime() - t0).toSeconds() < 5,
        "bounded by the deadline, not 50 attempts");
    assertTrue(calls("d1").get() < 10);
  }

  // ───── native unary cancel ──────────────────────────────────────────────

  @Test
  void cancellingACancellableUnaryCallReachesTheServerHandler() throws Exception {
    AsterClient c = client();
    CompletableFuture<Msg> f =
        c.call(
            addr,
            SVC,
            "slow",
            new Msg("", 0),
            Msg.class,
            CallOptions.DEFAULT.withCancellable(true));
    assertTrue(slowEntered.await(10, TimeUnit.SECONDS), "the handler is running");
    assertTrue(f.cancel(true));
    assertTrue(f.isCancelled());
    assertThrows(CancellationException.class, () -> f.get(5, TimeUnit.SECONDS));
    assertTrue(slowCancelled.await(10, TimeUnit.SECONDS), "the handler saw the cancel");
    assertEquals(1, SLOW_CANCELLED.get());
  }

  @Test
  void cancellingBeforeTheCallOpensDoesNotHangOrLeak() throws Exception {
    AsterClient c = client();
    for (int i = 0; i < 50; i++) {
      CompletableFuture<Msg> f =
          c.call(
              addr,
              SVC,
              "echo",
              new Msg("race", 0),
              Msg.class,
              CallOptions.DEFAULT.withCancellable(true));
      f.cancel(true);
      assertTrue(f.isCancelled());
    }
    // the client still works
    assertEquals(
        "echo:ok|", get(c.<Msg, Msg>call(addr, SVC, "echo", new Msg("ok", 0), Msg.class)).text());
  }

  @Test
  void aCancellableCallThatCompletesReturnsItsResult() throws Exception {
    AsterClient c = client();
    Msg out =
        get(
            c.<Msg, Msg>call(
                addr,
                SVC,
                "echo",
                new Msg("fast", 0),
                Msg.class,
                CallOptions.DEFAULT.withCancellable(true)));
    assertEquals("echo:fast|", out.text());
    Throwable t =
        failure(
            c.<Msg, Msg>call(
                addr,
                SVC,
                "boom",
                new Msg("cx", 0),
                Msg.class,
                CallOptions.DEFAULT.withCancellable(true)));
    assertEquals(StatusCode.INTERNAL, ((RpcError) t).code(), "status mapping on the open path");
  }

  @Test
  void cancelStopsRetriesAndBackoff() throws Exception {
    RetryInterceptor slowRetry =
        new RetryInterceptor(50, Set.of(StatusCode.UNAVAILABLE), 100, 100, 1.0, 0.0);
    AsterClient c = client(slowRetry);
    CompletableFuture<Msg> f =
        c.call(
            addr,
            SVC,
            "flaky",
            new Msg("cr", 999),
            Msg.class,
            CallOptions.DEFAULT.withIdempotent(true).withCancellable(true));
    Thread.sleep(300);
    assertTrue(f.cancel(true));
    int atCancel = calls("cr").get();
    Thread.sleep(500);
    assertTrue(calls("cr").get() <= atCancel + 1, "no further attempts after the cancel");
  }

  @Test
  void bufferedStreamingCallsCancelNatively() throws Exception {
    AsterClient c = client();
    CompletableFuture<List<Msg>> f =
        c.callServerStream(addr, SVC, "count", new Msg("c", 1_000_000), Msg.class);
    Thread.sleep(100);
    f.cancel(true);
    assertTrue(f.isCancelled());
    // the client keeps working afterwards
    assertEquals(
        2,
        get(c.<Msg, Msg>callServerStream(addr, SVC, "count", new Msg("c", 2), Msg.class)).size());
  }

  // ───── concurrency ──────────────────────────────────────────────────────

  @Test
  void concurrentCallsWithInterceptorsCancelsAndRetriesStayConsistent() throws Exception {
    CircuitBreakerInterceptor breaker = new CircuitBreakerInterceptor(1_000, 5.0, 1);
    AsterClient c = client(retry(4), breaker, new DeadlineInterceptor());
    int n = 64;
    List<CompletableFuture<Msg>> futures = new ArrayList<>();
    for (int i = 0; i < n; i++) {
      CallOptions o = CallOptions.DEFAULT.withIdempotent(true).withTimeout(Duration.ofSeconds(20));
      if (i % 4 == 0) {
        o = o.withCancellable(true);
      }
      futures.add(
          i % 2 == 0
              ? c.<Msg, Msg>call(addr, SVC, "flaky", new Msg("cc" + i, 2), Msg.class, o)
              : c.<Msg, Msg>call(addr, SVC, "echo", new Msg("e" + i, 0), Msg.class, o));
    }
    // cancel some of the cancellable ones while in flight
    for (int i = 0; i < n; i += 8) {
      futures.get(i).cancel(true);
    }
    for (int i = 0; i < n; i++) {
      CompletableFuture<Msg> f = futures.get(i);
      if (i % 8 == 0) {
        assertTrue(f.isCancelled() || f.isDone());
        continue;
      }
      Msg m = get(f);
      if (i % 2 == 0) {
        assertEquals("ok", m.text());
        assertEquals(2, m.n(), "one retry then success: " + i);
      } else {
        assertEquals("echo:e" + i + "|", m.text());
      }
    }
    assertEquals(CircuitBreakerInterceptor.State.CLOSED, breaker.state());
    assertFalse(breaker == null);
  }
}
