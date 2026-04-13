package site.aster.ffi;

import java.util.concurrent.*;
import site.aster.event.IrohEvent;

/**
 * Maps native {@code operation_id} values to Java {@link CompletableFuture} instances.
 *
 * <p>When an async FFI call is made, a future is registered here. When {@code iroh_poll_events}
 * returns a completion event, the poller looks up the future here and completes or fails it.
 */
public class OperationRegistry {

  private final ConcurrentHashMap<Long, CompletableFuture<?>[]> pending = new ConcurrentHashMap<>();

  /**
   * Register a new in-flight operation.
   *
   * @return the registered future
   */
  public CompletableFuture<IrohEvent> register(long operationId) {
    var future = new CompletableFuture<IrohEvent>();
    pending.put(operationId, new CompletableFuture<?>[] {future});
    return future;
  }

  /**
   * Register a future paired with a secondary future for split operations (e.g. open_bi returns
   * send+recv streams).
   */
  public CompletableFuture<?>[] registerWithSecondary(
      long operationId, CompletableFuture<?> primary, CompletableFuture<?> secondary) {
    pending.put(operationId, new CompletableFuture<?>[] {primary, secondary});
    return pending.get(operationId);
  }

  /** Complete a registered operation successfully. */
  public void complete(long operationId, IrohEvent event) {
    CompletableFuture<?>[] futures = pending.remove(operationId);
    if (futures != null) {
      ((CompletableFuture<IrohEvent>) futures[0]).complete(event);
    }
  }

  /** Fail a registered operation with an exception. */
  public void completeExceptionally(long operationId, Throwable t) {
    CompletableFuture<?>[] futures = pending.remove(operationId);
    if (futures != null) {
      ((CompletableFuture<IrohEvent>) futures[0]).completeExceptionally(t);
    }
  }

  /** Remove a registered operation without completing it (e.g. cancelled). */
  public CompletableFuture<?>[] remove(long operationId) {
    return pending.remove(operationId);
  }

  public int size() {
    return pending.size();
  }

  public void clear() {
    pending.clear();
  }
}
