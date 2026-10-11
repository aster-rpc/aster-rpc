package site.aster.docs;

import java.lang.foreign.Arena;
import java.lang.foreign.ValueLayout;
import java.time.Duration;
import java.util.concurrent.CancellationException;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.ScheduledThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicReference;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;

/**
 * Owned live receiver. One receive may be pending at a time. Close releases the native receiver and
 * wakes a blocked receive with EOF; an event already committed may still win a close race. The
 * receiver outlives its document handle.
 */
public final class DocEvents implements AutoCloseable {
  private static final ScheduledThreadPoolExecutor DEADLINES = deadlines();
  private final IrohRuntime runtime;
  private final long handle;
  private boolean closed;
  private Receive active;
  private DocEvent pending;

  private static ScheduledThreadPoolExecutor deadlines() {
    var executor =
        new ScheduledThreadPoolExecutor(
            1,
            task -> {
              Thread thread = new Thread(task, "aster-doc-deadlines");
              thread.setDaemon(true);
              return thread;
            });
    executor.setRemoveOnCancelPolicy(true);
    return executor;
  }

  DocEvents(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
  }

  static CompletableFuture<DocEvents> subscribe(Doc doc) {
    var rt = doc.runtime();
    var result = new CompletableFuture<DocEvents>();
    long operation;
    try (Arena a = Arena.ofConfined()) {
      var op = a.allocate(ValueLayout.JAVA_LONG);
      int status =
          IrohLibrary.getInstance().docSubscribe(rt.nativeHandle(), doc.docHandle(), 0L, op);
      if (status != 0) throw new IrohException(IrohStatus.fromCode(status), "subscribe failed");
      operation = op.get(ValueLayout.JAVA_LONG, 0);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(t);
    }
    rt.registry()
        .register(operation)
        .whenComplete(
            (event, error) -> {
              if (error != null) {
                result.completeExceptionally(error);
                return;
              }
              try {
                if (event.kind() != IrohEventKind.DOC_SUBSCRIBED)
                  throw new IrohException("unexpected subscribe event: " + event.kind());
                var owner = new DocEvents(rt, event.handle());
                if (!result.complete(owner)) owner.close();
              } catch (Throwable t) {
                result.completeExceptionally(t);
              } finally {
                if (event.hasBuffer()) rt.releaseBuffer(event.buffer());
              }
            });
    return result;
  }

  /**
   * Receive a change, or null on EOF. Canceling the future also requests native cancellation. A
   * successful event committed before cancellation is retained for the next receive.
   */
  public CompletableFuture<DocEvent> nextAsync() {
    return nextAsync(null);
  }

  /**
   * Receive with a native cancellation deadline. If the event already committed when cancellation
   * races it, the event wins. A timeout does not consume the next event. Use this deadline instead
   * of CompletableFuture.orTimeout, which completes without settling the native operation.
   */
  public synchronized CompletableFuture<DocEvent> nextAsync(Duration timeout) {
    if (closed) return CompletableFuture.completedFuture(null);
    if (timeout != null && (timeout.isNegative() || timeout.isZero()))
      return CompletableFuture.failedFuture(
          new IllegalArgumentException("timeout must be positive"));
    final long timeoutNanos;
    try {
      timeoutNanos = timeout == null ? 0 : timeout.toNanos();
    } catch (ArithmeticException e) {
      return CompletableFuture.failedFuture(e);
    }
    if (active != null) {
      if (active.isCancelled()) return afterSettled(active, timeout);
      return CompletableFuture.failedFuture(new IllegalStateException("receive already pending"));
    }
    if (pending != null) {
      DocEvent event = pending;
      pending = null;
      return CompletableFuture.completedFuture(event);
    }
    long operation;
    try (Arena a = Arena.ofConfined()) {
      var op = a.allocate(ValueLayout.JAVA_LONG);
      int status = IrohLibrary.getInstance().docEventRecv(runtime.nativeHandle(), handle, 0L, op);
      if (status == IrohStatus.NOT_FOUND.code) {
        closed = true;
        return CompletableFuture.completedFuture(null);
      }
      if (status != 0)
        throw new IrohException(IrohStatus.fromCode(status), "document receive failed");
      operation = op.get(ValueLayout.JAVA_LONG, 0);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(t);
    }
    Receive result = new Receive(operation);
    active = result;
    result.whenComplete(
        (value, error) -> {
          result.cancelTimer();
          if (result.isCancelled()) result.requestCancel(false);
        });
    runtime
        .registry()
        .register(operation)
        .whenComplete(
            (event, error) -> {
              DocEvent value = null;
              Throwable failure = error;
              result.terminal = true;
              try {
                if (error instanceof IrohException e && e.status() == IrohStatus.CANCELLED)
                  failure =
                      result.timedOut
                          ? new TimeoutException("document receive timed out")
                          : new CancellationException("document receive canceled");
                if (event != null) {
                  if (event.kind() != IrohEventKind.DOC_EVENT)
                    throw new IrohException("unexpected document event: " + event.kind());
                  if (event.status() == IrohStatus.NOT_FOUND.code) {
                    synchronized (this) {
                      closed = true;
                    }
                  } else if (event.status() != 0)
                    throw new IrohException(
                        IrohStatus.fromCode(event.status()), "document event failed");
                  else {
                    byte[] payload =
                        event.dataLen() == 0
                            ? new byte[0]
                            : event
                                .data()
                                .asSlice(0, event.dataLen())
                                .toArray(ValueLayout.JAVA_BYTE);
                    value = DocEvent.decode(event.flags(), payload);
                  }
                }
              } catch (Throwable t) {
                failure = t;
              } finally {
                if (event != null && event.hasBuffer()) {
                  try {
                    runtime.releaseBuffer(event.buffer());
                  } catch (RuntimeException e) {
                    failure = e;
                  }
                }
              }
              synchronized (this) {
                active = null;
                if (failure == null) {
                  if (!result.complete(value) && value != null && !closed) pending = value;
                } else result.completeExceptionally(failure);
                result.settled.complete(null);
              }
            });
    if (timeout != null) {
      result.timer =
          DEADLINES.schedule(() -> result.requestCancel(true), timeoutNanos, TimeUnit.NANOSECONDS);
      if (result.isDone()) result.cancelTimer();
    }
    return result;
  }

  private CompletableFuture<DocEvent> afterSettled(Receive previous, Duration timeout) {
    var result = new CompletableFuture<DocEvent>();
    var delegate = new AtomicReference<CompletableFuture<DocEvent>>();
    result.whenComplete(
        (value, error) -> {
          CompletableFuture<DocEvent> current = delegate.get();
          if (result.isCancelled() && current != null) current.cancel(false);
        });
    previous.settled.whenComplete(
        (ignored, error) -> {
          synchronized (this) {
            if (result.isCancelled()) return;
            CompletableFuture<DocEvent> current = nextAsync(timeout);
            delegate.set(current);
            if (result.isCancelled()) current.cancel(false);
            current.whenComplete(
                (event, failure) -> {
                  synchronized (this) {
                    if (failure != null) result.completeExceptionally(failure);
                    else if (!result.complete(event) && event != null && !closed) pending = event;
                  }
                });
          }
        });
    return result;
  }

  @Override
  public synchronized void close() {
    if (closed) return;
    int status = IrohLibrary.getInstance().docEventReceiverFree(runtime.nativeHandle(), handle);
    if (status != 0
        && status != IrohStatus.NOT_FOUND.code
        && status != IrohStatus.ALREADY_CLOSED.code)
      throw new IrohException(IrohStatus.fromCode(status), "receiver free failed");
    closed = true;
    pending = null;
  }

  private final class Receive extends CompletableFuture<DocEvent> {
    private final long operation;
    private volatile boolean timedOut;
    private volatile boolean terminal;
    private volatile ScheduledFuture<?> timer;
    private final CompletableFuture<Void> settled = new CompletableFuture<>();
    private boolean cancelRequested;

    Receive(long operation) {
      this.operation = operation;
    }

    synchronized boolean requestCancel(boolean timeout) {
      if (terminal || cancelRequested || (timeout && isDone())) return false;
      cancelRequested = true;
      timedOut = timeout;
      try {
        runtime.cancelOperation(operation);
      } catch (IrohException e) {
        // A committed event can leave the native registry before the poller
        // delivers it. Keep waiting for that completion rather than lose it.
        if (e.status() != IrohStatus.NOT_FOUND) throw e;
      }
      return true;
    }

    void cancelTimer() {
      ScheduledFuture<?> task = timer;
      if (task != null) task.cancel(false);
    }
  }
}
