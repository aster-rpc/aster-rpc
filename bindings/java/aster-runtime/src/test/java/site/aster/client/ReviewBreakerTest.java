package site.aster.client;

import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.util.List;
import java.util.concurrent.CancellationException;
import java.util.concurrent.atomic.AtomicBoolean;
import org.junit.jupiter.api.Test;
import site.aster.interceptors.CircuitBreakerInterceptor;
import site.aster.interceptors.Interceptor;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;

final class ReviewBreakerTest {
  private static CircuitBreakerInterceptor open(double recoverySeconds) {
    var b = new CircuitBreakerInterceptor(1, recoverySeconds, 1);
    b.recordFailure(new RpcError(StatusCode.UNAVAILABLE, "down"));
    return b;
  }

  @Test
  void refusalReleasesPreviouslyAdmittedProbe() {
    var probe = open(0);
    List<Interceptor> chain = List.of(probe, open(3600));
    var options = CallOptions.DEFAULT;
    var ctx = Interception.context(chain, "Echo", "echo", 0, null, "unary", options);
    var error =
        assertThrows(
            RpcError.class,
            () ->
                Interception.execute(
                    chain,
                    ctx,
                    options,
                    "request",
                    false,
                    () -> false,
                    (req, deadline, metadata) -> {
                      throw new AssertionError("refused call reached transport");
                    }));
    assertEquals(StatusCode.UNAVAILABLE, error.code());
    assertDoesNotThrow(probe::beforeCall, "unused half-open slot must be returned");
  }

  @Test
  void callerCancellationReleasesProbe() {
    var probe = open(0);
    List<Interceptor> chain = List.of(probe);
    var options = CallOptions.DEFAULT;
    var ctx = Interception.context(chain, "Echo", "echo", 0, null, "unary", options);
    var cancelled = new AtomicBoolean();
    assertThrows(
        CancellationException.class,
        () ->
            Interception.execute(
                chain,
                ctx,
                options,
                "request",
                false,
                cancelled::get,
                (req, deadline, metadata) -> {
                  cancelled.set(true);
                  throw new CancellationException("caller stopped");
                }));
    assertDoesNotThrow(probe::beforeCall, "cancelled probe must not strand the circuit");
  }
}
