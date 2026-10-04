package site.aster.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CancellationException;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;
import site.aster.interceptors.CallContext;
import site.aster.interceptors.CircuitBreakerInterceptor;
import site.aster.interceptors.DeadlineInterceptor;
import site.aster.interceptors.Interceptor;
import site.aster.interceptors.RetryInterceptor;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;

/** The interceptor lifecycle ({@link Interception}) without a network. */
final class InterceptionTest {

  private static CallOptions idempotent() {
    return CallOptions.DEFAULT.withIdempotent(true);
  }

  private static RetryInterceptor retry(int attempts) {
    return new RetryInterceptor(attempts, Set.of(StatusCode.UNAVAILABLE), 1, 2, 1.0, 0.0);
  }

  @Test
  void callOptionsAreImmutableCopies() {
    CallOptions base = CallOptions.DEFAULT;
    CallOptions o =
        base.withTimeout(Duration.ofSeconds(3)).withIdempotent(true).withMetadata("k", "v");
    assertNull(base.timeout());
    assertFalse(base.idempotent());
    assertTrue(base.metadata().isEmpty());
    assertEquals(Duration.ofSeconds(3), o.timeout());
    assertEquals(Map.of("k", "v"), o.metadata());
    assertNull(o.withTimeout(Duration.ZERO).timeout(), "non-positive means no deadline");
    assertNull(o.withTimeout(null).timeout());
  }

  @Test
  void noInterceptorsRunsTheAttemptDirectlyWithTheOptionsDeadline() throws Throwable {
    CallOptions o = CallOptions.DEFAULT.withTimeout(Duration.ofMillis(1500)).withMetadata("a", "b");
    int[] seen = new int[1];
    Object out =
        Interception.execute(
            List.of(),
            null,
            o,
            "req",
            false,
            () -> false,
            (req, deadlineMs, metadata) -> {
              seen[0] = deadlineMs;
              assertEquals("req", req);
              assertEquals(Map.of("a", "b"), metadata);
              return "resp";
            });
    assertEquals("resp", out);
    assertEquals(1500, seen[0]);
    // failures keep their original type
    IllegalStateException boom = new IllegalStateException("x");
    assertSame(
        boom,
        assertThrows(
            IllegalStateException.class,
            () ->
                Interception.execute(
                    List.of(),
                    null,
                    o,
                    "req",
                    false,
                    () -> false,
                    (req, d, m) -> {
                      throw boom;
                    })));
  }

  @Test
  void contextCarriesPatternIdempotencyDeadlineAndMetadata() {
    CallContext ctx =
        Interception.context(
            List.of(new DeadlineInterceptor()),
            "S",
            "m",
            7,
            "peer1",
            "server_stream",
            idempotent().withTimeout(Duration.ofSeconds(10)).withMetadata("k", "v"));
    assertEquals("server_stream", ctx.pattern());
    assertTrue(ctx.isStreaming());
    assertTrue(ctx.isIdempotent());
    assertEquals("7", ctx.sessionId());
    assertEquals("peer1", ctx.peer());
    assertEquals("v", ctx.metadata().get("k"));
    assertTrue(ctx.remainingSeconds() > 8 && ctx.remainingSeconds() <= 10);
    int ms = Interception.deadlineMs(ctx, CallOptions.DEFAULT);
    assertTrue(ms > 8000 && ms <= 10_000, "remaining, not the original: " + ms);
    assertNull(Interception.context(List.of(), "S", "m", 0, "p", "unary", CallOptions.DEFAULT));
  }

  @Test
  void retriesOnlyIdempotentRetryableFailuresUpToMaxAttempts() throws Throwable {
    List<Interceptor> chain = List.of(retry(4));
    AtomicInteger calls = new AtomicInteger();
    CallContext ctx = Interception.context(chain, "S", "m", 0, "p", "unary", idempotent());
    String out =
        Interception.execute(
            chain,
            ctx,
            idempotent(),
            "r",
            false,
            () -> false,
            (req, d, m) -> {
              if (calls.incrementAndGet() < 3) {
                throw new RpcError(StatusCode.UNAVAILABLE, "no");
              }
              return "done";
            });
    assertEquals("done", out);
    assertEquals(3, calls.get());
    assertEquals(3, ctx.attempt());

    // not idempotent: one attempt
    calls.set(0);
    CallContext plain = Interception.context(chain, "S", "m", 0, "p", "unary", CallOptions.DEFAULT);
    assertThrows(
        RpcError.class,
        () ->
            Interception.execute(
                chain,
                plain,
                CallOptions.DEFAULT,
                "r",
                false,
                () -> false,
                (req, d, m) -> {
                  calls.incrementAndGet();
                  throw new RpcError(StatusCode.UNAVAILABLE, "no");
                }));
    assertEquals(1, calls.get());

    // exhausted: the last error, exactly maxAttempts attempts
    calls.set(0);
    CallContext ctx2 = Interception.context(chain, "S", "m", 0, "p", "unary", idempotent());
    RpcError e =
        assertThrows(
            RpcError.class,
            () ->
                Interception.execute(
                    chain,
                    ctx2,
                    idempotent(),
                    "r",
                    false,
                    () -> false,
                    (req, d, m) -> {
                      calls.incrementAndGet();
                      throw new RpcError(StatusCode.UNAVAILABLE, "always");
                    }));
    assertEquals(StatusCode.UNAVAILABLE, e.code());
    assertEquals(4, calls.get());
  }

  @Test
  void nonRpcFailuresAreNormalisedWhenInterceptorsRun() {
    List<Interceptor> chain = List.of(new Interceptor() {});
    CallContext ctx = Interception.context(chain, "S", "m", 0, "p", "unary", CallOptions.DEFAULT);
    RpcError e =
        assertThrows(
            RpcError.class,
            () ->
                Interception.execute(
                    chain,
                    ctx,
                    CallOptions.DEFAULT,
                    "r",
                    false,
                    () -> false,
                    (req, d, m) -> {
                      throw new IllegalStateException("kaput");
                    }));
    assertEquals(StatusCode.UNKNOWN, e.code());
    assertTrue(e.rpcMessage().contains("kaput"));
    assertEquals(StatusCode.CANCELLED, Interception.normalize(new CancellationException()).code());
    assertEquals(
        StatusCode.DEADLINE_EXCEEDED, Interception.normalize(new TimeoutException()).code());
  }

  @Test
  void breakerRejectionIsNotRetriedNorCountedAndSuccessCloses() throws Throwable {
    CircuitBreakerInterceptor breaker = new CircuitBreakerInterceptor(1, 60.0, 1);
    List<Interceptor> chain = List.of(retry(5), breaker);
    AtomicInteger calls = new AtomicInteger();
    CallContext ctx = Interception.context(chain, "S", "m", 0, "p", "unary", idempotent());
    // attempt 1 fails -> opens (threshold 1); attempt 2's beforeCall rejects, ending the call
    RpcError e =
        assertThrows(
            RpcError.class,
            () ->
                Interception.execute(
                    chain,
                    ctx,
                    idempotent(),
                    "r",
                    false,
                    () -> false,
                    (req, d, m) -> {
                      calls.incrementAndGet();
                      throw new RpcError(StatusCode.UNAVAILABLE, "down");
                    }));
    assertTrue(e.rpcMessage().contains("circuit breaker is open"));
    assertEquals(1, calls.get());
    assertEquals(CircuitBreakerInterceptor.State.OPEN, breaker.state());
  }

  @Test
  void breakerStateMachine() throws Exception {
    CircuitBreakerInterceptor b = new CircuitBreakerInterceptor(2, 0.1, 1);
    b.recordFailure(new RpcError(StatusCode.NOT_FOUND, "ignored"));
    b.recordFailure(new RpcError(StatusCode.UNAVAILABLE, "1"));
    assertEquals(CircuitBreakerInterceptor.State.CLOSED, b.state());
    b.recordFailure(new RpcError(StatusCode.INTERNAL, "2"));
    assertEquals(CircuitBreakerInterceptor.State.OPEN, b.state());
    assertThrows(RpcError.class, b::beforeCall);
    Thread.sleep(150);
    b.beforeCall(); // half-open probe
    assertEquals(CircuitBreakerInterceptor.State.HALF_OPEN, b.state());
    assertThrows(RpcError.class, b::beforeCall, "probe budget spent");
    b.recordSuccess();
    assertEquals(CircuitBreakerInterceptor.State.CLOSED, b.state());
  }

  @Test
  void aCancelledCallIsNotRetried() {
    List<Interceptor> chain = List.of(retry(5));
    AtomicBoolean cancelled = new AtomicBoolean();
    AtomicInteger calls = new AtomicInteger();
    CallContext ctx = Interception.context(chain, "S", "m", 0, "p", "unary", idempotent());
    assertThrows(
        RpcError.class,
        () ->
            Interception.execute(
                chain,
                ctx,
                idempotent(),
                "r",
                false,
                cancelled::get,
                (req, d, m) -> {
                  calls.incrementAndGet();
                  cancelled.set(true);
                  throw new RpcError(StatusCode.UNAVAILABLE, "down");
                }));
    assertEquals(1, calls.get());
  }

  @Test
  void callContextCancellationDefaultsToFalse() {
    assertFalse(CallContext.builder("S", "m").build().isCancelled());
    AtomicBoolean flag = new AtomicBoolean();
    CallContext ctx = CallContext.builder("S", "m").cancellation(flag::get).build();
    assertFalse(ctx.isCancelled());
    flag.set(true);
    assertTrue(ctx.isCancelled());
  }
}
