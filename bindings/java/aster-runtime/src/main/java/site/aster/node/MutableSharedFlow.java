package site.aster.node;

import java.util.List;
import java.util.concurrent.*;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantLock;
import java.util.function.Consumer;

/**
 * A hot flow that broadcasts values to all collectors.
 *
 * <p>This is a simplified implementation of kotlinx.coroutines.flow.MutableSharedFlow for use in
 * the Java FFI bindings.
 */
public class MutableSharedFlow<T> implements Flow<T> {

  private final int replay;
  private final int extraBufferCapacity;
  private final BufferOverflow overflow;
  private final ConcurrentLinkedQueue<T> buffer;
  private final List<Consumer<T>> collectors;
  private final ReentrantLock lock;
  private final Condition notEmpty;
  private volatile boolean closed = false;

  public MutableSharedFlow(int replay, int extraBufferCapacity, BufferOverflow overflow) {
    this.replay = replay;
    this.extraBufferCapacity = extraBufferCapacity;
    this.overflow = overflow;
    this.buffer = new ConcurrentLinkedQueue<>();
    this.collectors = new CopyOnWriteArrayList<>();
    this.lock = new ReentrantLock();
    this.notEmpty = lock.newCondition();
  }

  /**
   * Emit a value to all collectors.
   *
   * @param value the value to emit
   */
  public void emit(T value) {
    if (closed) return;

    lock.lock();
    try {
      // Add to buffer
      if (buffer.size() >= replay + extraBufferCapacity) {
        switch (overflow) {
          case DROP_OLDEST:
            buffer.poll();
            break;
          case DROP_NEWEST:
            // Don't add the new value
            return;
          case SUSPEND:
            while (buffer.size() >= replay + extraBufferCapacity) {
              try {
                notEmpty.await();
              } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return;
              }
            }
            break;
        }
      }

      buffer.offer(value);
      notEmpty.signalAll();
    } finally {
      lock.unlock();
    }

    // Notify collectors outside the lock
    List<Consumer<T>> snapshot = collectors;
    for (Consumer<T> collector : snapshot) {
      try {
        collector.accept(value);
      } catch (Exception e) {
        // Ignore collector errors
      }
    }
  }

  @Override
  public CompletableFuture<Void> collect(
      java.util.function.Consumer<T> collector, Executor executor) {
    collectors.add(collector);
    CompletableFuture<Void> future = new CompletableFuture<>();

    executor.execute(
        () -> {
          lock.lock();
          try {
            // Emit replayed values
            Object[] buffered = buffer.toArray();
            int toReplay = Math.min(replay, buffered.length);
            for (int i = 0; i < toReplay; i++) {
              @SuppressWarnings("unchecked")
              T value = (T) buffered[i];
              collector.accept(value);
            }
          } finally {
            lock.unlock();
          }

          // Keep collecting until cancelled or closed
          while (!closed && !Thread.currentThread().isInterrupted()) {
            lock.lock();
            try {
              while (buffer.isEmpty() && !closed) {
                notEmpty.await();
              }
              if (closed) break;
              T value = buffer.poll();
              if (value != null) {
                notEmpty.signalAll();
                collector.accept(value);
              }
            } catch (InterruptedException e) {
              Thread.currentThread().interrupt();
              break;
            } finally {
              lock.unlock();
            }
          }

          collectors.remove(collector);
          future.complete(null);
        });

    return future;
  }
}
