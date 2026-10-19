package site.aster.client;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CancellationException;
import java.util.function.BooleanSupplier;
import site.aster.interceptors.CallContext;
import site.aster.interceptors.CircuitBreakerInterceptor;
import site.aster.interceptors.Interceptor;
import site.aster.interceptors.InterceptorChain;
import site.aster.interceptors.InterceptorClock;
import site.aster.interceptors.RetryInterceptor;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;

/**
 * Drives {@link AsterClient}'s interceptors around a call, following Python's {@code
 * AsterClient._run_call_with_interceptors} (the reference; Rust's {@code Pipeline} is unary-only):
 *
 * <ol>
 *   <li>per attempt: every {@link CircuitBreakerInterceptor#beforeCall()} (an open circuit rejects
 *       the call outright: no retry, not counted as a failure), then the request interceptors in
 *       order ({@code DeadlineInterceptor} refuses an expired call here), then the attempt itself
 *       with the <em>remaining</em> deadline propagated natively;
 *   <li>success: the response interceptors in order, then {@link
 *       CircuitBreakerInterceptor#recordSuccess()};
 *   <li>failure: the error is normalised to an {@link RpcError}, the breakers record it, the error
 *       interceptors run in reverse order (one returning {@code null} suppresses the error and the
 *       call yields {@code null}), then a {@link RetryInterceptor} that allows it (idempotent call,
 *       retryable code, attempts left) backs off (bounded by the deadline) and repeats.
 * </ol>
 *
 * Retry and breaker apply to unary and client-streaming calls only; the other patterns use {@link
 * #request}, {@link #response} and {@link #error} directly. Without interceptors nothing here runs
 * and failures keep their original type.
 */
final class Interception {

  private Interception() {}

  /** One attempt: the post-interceptor request, the deadline to send (0 = none) and metadata. */
  @FunctionalInterface
  interface Attempt<T> {
    T run(Object request, int deadlineMs, Map<String, String> metadata) throws Throwable;
  }

  /** The new per-call context; {@code null} when there is nothing to intercept. */
  static CallContext context(
      List<Interceptor> chain,
      String service,
      String method,
      int sessionId,
      String peer,
      String pattern,
      CallOptions options) {
    if (chain.isEmpty()) {
      return null;
    }
    CallContext.Builder b =
        CallContext.builder(service, method)
            .metadata(options.metadata())
            .pattern(pattern)
            .streaming(!"unary".equals(pattern))
            .idempotent(options.idempotent())
            .peer(peer);
    if (sessionId != 0) {
      b.sessionId(Integer.toString(sessionId));
    }
    if (options.timeout() != null) {
      b.deadline(System.currentTimeMillis() / 1000.0 + options.timeout().toNanos() / 1e9);
    }
    return b.build();
  }

  /** Relative deadline in ms for the wire: what remains of {@code ctx}'s, at least 1; 0 = none. */
  static int deadlineMs(CallContext ctx, CallOptions options) {
    Double remaining = ctx != null ? ctx.remainingSeconds() : null;
    if (remaining == null) {
      if (ctx != null || options.timeout() == null) {
        return 0;
      }
      remaining = options.timeout().toNanos() / 1e9;
    }
    return wireMs(remaining);
  }

  private static int wireMs(double remainingSeconds) {
    return (int) Math.min(Integer.MAX_VALUE, Math.max(1.0, Math.ceil(remainingSeconds * 1000.0)));
  }

  static Map<String, String> metadata(CallContext ctx, CallOptions options) {
    return ctx != null ? ctx.metadata() : options.metadata();
  }

  /**
   * Run {@code attempt} under the full lifecycle (retry + breaker + deadline); {@code perItem}
   * applies the request interceptors to each element of the {@code request} list (client-stream).
   */
  @SuppressWarnings("unchecked")
  static <T> T execute(
      List<Interceptor> chain,
      CallContext ctx,
      CallOptions options,
      Object request,
      boolean perItem,
      BooleanSupplier cancelled,
      Attempt<T> attempt)
      throws Throwable {
    if (ctx == null) {
      return attempt.run(request, deadlineMs(null, options), options.metadata());
    }
    List<RetryInterceptor> retries = new ArrayList<>();
    List<CircuitBreakerInterceptor> breakers = new ArrayList<>();
    int maxAttempts = 1;
    for (Interceptor i : chain) {
      if (i instanceof RetryInterceptor r) {
        retries.add(r);
        maxAttempts = Math.max(maxAttempts, r.maxAttempts());
      } else if (i instanceof CircuitBreakerInterceptor b) {
        breakers.add(b);
      }
    }

    // The deadline budget runs on the retry clock (the system clock unless a test injects one), so
    // the backoff and the deadline it is bounded by share one time source.
    InterceptorClock clock = retries.isEmpty() ? InterceptorClock.SYSTEM : retries.get(0).clock();
    Double budget = ctx.remainingSeconds();
    long deadlineAt = budget == null ? 0L : clock.nanoTime() + (long) (budget * 1e9);

    RpcError last = null;
    for (int n = 1; n <= maxAttempts; n++) {
      if (cancelled.getAsBoolean()) {
        throw new CancellationException("call cancelled");
      }
      ctx.setAttempt(n);
      admit(breakers);
      try {
        Object current =
            perItem
                ? requestEach(chain, ctx, (List<Object>) request)
                : request(chain, ctx, request);
        int wire = budget == null ? 0 : wireMs((deadlineAt - clock.nanoTime()) / 1e9);
        T response = attempt.run(current, wire, ctx.metadata());
        response = (T) response(chain, ctx, response);
        for (CircuitBreakerInterceptor b : breakers) {
          b.recordSuccess();
        }
        return response;
      } catch (Throwable t) {
        if (cancelled.getAsBoolean()) {
          // No verdict on the peer: give back any half-open probe slot.
          for (CircuitBreakerInterceptor b : breakers) {
            b.releaseProbe();
          }
          throw t;
        }
        RpcError error = normalize(t);
        for (CircuitBreakerInterceptor b : breakers) {
          b.recordFailure(error);
        }
        RpcError maybe = InterceptorChain.applyError(chain, ctx, error);
        if (maybe == null) {
          return null; // an error interceptor suppressed it
        }
        last = maybe;
        boolean retry = false;
        double delay = 0.0;
        for (RetryInterceptor r : retries) {
          if (r.shouldRetry(ctx, maybe) && n < r.maxAttempts()) {
            retry = true;
            delay = Math.max(delay, r.backoffSeconds(n));
          }
        }
        if (!retry) {
          throw maybe;
        }
        sleep(delay, clock, budget == null ? null : deadlineAt, cancelled);
      }
    }
    throw last != null ? last : new RpcError(StatusCode.INTERNAL, "call failed");
  }

  /** Consult every breaker; if one refuses, give back the slots the others took. */
  private static void admit(List<CircuitBreakerInterceptor> breakers) {
    for (int i = 0; i < breakers.size(); i++) {
      try {
        breakers.get(i).beforeCall();
      } catch (RuntimeException e) {
        for (int j = 0; j < i; j++) {
          breakers.get(j).releaseProbe();
        }
        throw e;
      }
    }
  }

  /** Request interceptors in order. */
  static Object request(List<Interceptor> chain, CallContext ctx, Object request) {
    return InterceptorChain.applyRequest(chain, ctx, request);
  }

  static List<Object> requestEach(List<Interceptor> chain, CallContext ctx, List<Object> requests) {
    List<Object> out = new ArrayList<>(requests.size());
    for (Object r : requests) {
      out.add(InterceptorChain.applyRequest(chain, ctx, r));
    }
    return out;
  }

  /** Response interceptors in order. */
  static Object response(List<Interceptor> chain, CallContext ctx, Object response) {
    return InterceptorChain.applyResponse(chain, ctx, response);
  }

  /**
   * The error to surface for {@code t} after the error interceptors (reverse order): {@code null}
   * if one suppressed it. Without interceptors {@code t} itself.
   */
  static Throwable error(List<Interceptor> chain, CallContext ctx, Throwable t) {
    if (ctx == null) {
      return t;
    }
    return InterceptorChain.applyError(chain, ctx, normalize(t));
  }

  /** Any failure as an {@link RpcError} (Python's {@code normalize_error}). */
  static RpcError normalize(Throwable t) {
    Throwable cause =
        t instanceof java.util.concurrent.CompletionException && t.getCause() != null
            ? t.getCause()
            : t;
    if (cause instanceof RpcError e) {
      return e;
    }
    if (cause instanceof CancellationException) {
      return new RpcError(StatusCode.CANCELLED, "call cancelled");
    }
    if (cause instanceof java.util.concurrent.TimeoutException) {
      return new RpcError(StatusCode.DEADLINE_EXCEEDED, "deadline exceeded");
    }
    String msg = cause.getMessage();
    return new RpcError(StatusCode.UNKNOWN, msg == null ? cause.toString() : msg);
  }

  /**
   * Back off for {@code seconds} of {@code clock}, never past {@code deadlineAt} (a {@code clock}
   * time; {@code null} = no deadline), waking early if cancelled.
   */
  private static void sleep(
      double seconds, InterceptorClock clock, Long deadlineAt, BooleanSupplier cancelled) {
    long end = clock.nanoTime() + (long) (seconds * 1e9);
    while (true) {
      long now = clock.nanoTime();
      Long remaining = deadlineAt == null ? null : deadlineAt - now;
      if (remaining != null && remaining <= 0) {
        throw new RpcError(StatusCode.DEADLINE_EXCEEDED, "deadline exceeded");
      }
      long left = end - now;
      if (left <= 0) {
        return;
      }
      if (cancelled.getAsBoolean()) {
        throw new CancellationException("call cancelled");
      }
      long slice = Math.min(left, 20_000_000L);
      if (remaining != null) {
        slice = Math.min(slice, remaining + 1_000_000L);
      }
      try {
        clock.sleep(slice);
      } catch (InterruptedException e) {
        Thread.currentThread().interrupt();
        throw new RpcError(StatusCode.CANCELLED, "interrupted while backing off");
      }
    }
  }
}
