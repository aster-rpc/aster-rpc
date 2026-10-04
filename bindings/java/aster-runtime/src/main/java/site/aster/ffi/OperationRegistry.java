package site.aster.ffi;

import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import site.aster.event.IrohEvent;

/**
 * Maps native {@code operation_id} values to Java {@link CompletableFuture} instances.
 *
 * <p>When an async FFI call is made, a future is registered here. When {@code aster_poll_events}
 * returns a completion event, the poller looks up the future here and completes or fails it.
 *
 * <p>The operation id is only known once the native call has returned, so {@link #register} runs
 * <em>after</em> the native side has started the operation. An operation that finishes quickly (a
 * fast native worker, or a Java thread descheduled on a CPU-starved host) can therefore have its
 * terminal event polled and dispatched <em>before</em> its future exists. Such an early outcome is
 * parked here and handed to the future the moment it registers; dropping it would leave the future
 * pending forever.
 */
public class OperationRegistry {

  /** Upper bound on parked outcomes whose operation never registered (e.g. fire-and-forget). */
  private static final int MAX_EARLY = 4096;

  private final Object lock = new Object();
  private final Map<Long, CompletableFuture<?>[]> pending = new HashMap<>();

  /**
   * Outcomes that arrived before their registration: an {@link IrohEvent} or a {@link Throwable}.
   */
  private final Map<Long, Object> early =
      new LinkedHashMap<>() {
        @Override
        protected boolean removeEldestEntry(Map.Entry<Long, Object> eldest) {
          return size() > MAX_EARLY;
        }
      };

  /**
   * Register a new in-flight operation.
   *
   * @return the registered future
   */
  public CompletableFuture<IrohEvent> register(long operationId) {
    var future = new CompletableFuture<IrohEvent>();
    deliverIfEarly(operationId, new CompletableFuture<?>[] {future});
    return future;
  }

  /**
   * Register a future paired with a secondary future for split operations (e.g. open_bi returns
   * send+recv streams).
   */
  public CompletableFuture<?>[] registerWithSecondary(
      long operationId, CompletableFuture<?> primary, CompletableFuture<?> secondary) {
    var futures = new CompletableFuture<?>[] {primary, secondary};
    deliverIfEarly(operationId, futures);
    return futures;
  }

  /** Complete a registered operation successfully. */
  @SuppressWarnings("unchecked")
  public void complete(long operationId, IrohEvent event) {
    CompletableFuture<?>[] futures = take(operationId, event);
    if (futures != null) {
      ((CompletableFuture<IrohEvent>) futures[0]).complete(event);
    }
  }

  /** Fail a registered operation with an exception. */
  @SuppressWarnings("unchecked")
  public void completeExceptionally(long operationId, Throwable t) {
    CompletableFuture<?>[] futures = take(operationId, t);
    if (futures != null) {
      ((CompletableFuture<IrohEvent>) futures[0]).completeExceptionally(t);
    }
  }

  /** Remove a registered operation without completing it (e.g. cancelled). */
  public CompletableFuture<?>[] remove(long operationId) {
    synchronized (lock) {
      early.remove(operationId);
      return pending.remove(operationId);
    }
  }

  public int size() {
    synchronized (lock) {
      return pending.size();
    }
  }

  public void clear() {
    synchronized (lock) {
      pending.clear();
      early.clear();
    }
  }

  /**
   * Take the future waiting on {@code operationId}; with none registered yet, park {@code outcome}
   * for the registration that is about to follow and return {@code null}.
   */
  private CompletableFuture<?>[] take(long operationId, Object outcome) {
    synchronized (lock) {
      CompletableFuture<?>[] futures = pending.remove(operationId);
      if (futures == null && operationId != 0) {
        early.put(operationId, outcome);
      }
      return futures;
    }
  }

  /**
   * Publish {@code futures} for {@code operationId}, or finish them now if the outcome is parked.
   */
  @SuppressWarnings("unchecked")
  private void deliverIfEarly(long operationId, CompletableFuture<?>[] futures) {
    Object outcome;
    synchronized (lock) {
      outcome = early.remove(operationId);
      if (outcome == null) {
        pending.put(operationId, futures);
        return;
      }
    }
    if (outcome instanceof Throwable t) {
      ((CompletableFuture<IrohEvent>) futures[0]).completeExceptionally(t);
    } else {
      ((CompletableFuture<IrohEvent>) futures[0]).complete((IrohEvent) outcome);
    }
  }
}
