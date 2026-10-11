package site.aster.blobs;

import java.lang.foreign.ValueLayout;
import java.util.concurrent.CancellationException;
import java.util.concurrent.CompletableFuture;
import java.util.function.Function;
import site.aster.event.IrohEvent;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;

/** A cooperative operation whose terminal result acknowledges cleanup or an irrevocable commit. */
public final class BlobOperation<T> extends CompletableFuture<T> {
  private final IrohRuntime runtime;
  private final long operation;
  private boolean terminal;
  private boolean requested;

  BlobOperation(IrohRuntime runtime, long operation, Function<IrohEvent, T> decode) {
    this.runtime = runtime;
    this.operation = operation;
    runtime
        .registry()
        .register(operation)
        .whenComplete(
            (event, failure) -> {
              synchronized (this) {
                terminal = true;
              }
              T value = null;
              try {
                if (failure instanceof IrohException e && e.status() == IrohStatus.CANCELLED)
                  failure =
                      new CancellationException("blob operation canceled after native cleanup");
                if (failure == null) value = decode.apply(event);
              } catch (Throwable error) {
                failure = error;
              } finally {
                if (event != null && event.hasBuffer()) {
                  try {
                    runtime.releaseBuffer(event.buffer());
                  } catch (IrohException e) {
                    if (e.status() != IrohStatus.ALREADY_CLOSED
                        && e.status() != IrohStatus.NOT_FOUND) failure = e;
                  }
                }
              }
              if (failure == null) complete(value);
              else completeExceptionally(failure);
            });
  }

  /** Request cancellation, retaining the terminal registration. A committed tag result may win. */
  public synchronized boolean requestCancel() {
    if (terminal || requested) return false;
    requested = true;
    try {
      runtime.cancelOperation(operation);
    } catch (IrohException e) {
      if (e.status() != IrohStatus.NOT_FOUND && e.status() != IrohStatus.ALREADY_CLOSED) throw e;
    }
    return true;
  }

  /**
   * Immediate CompletableFuture cancellation is unsupported; use requestCancel and await this
   * operation.
   */
  @Override
  public boolean cancel(boolean mayInterruptIfRunning) {
    return false;
  }

  static byte[] payload(IrohEvent event) {
    return event.dataLen() == 0
        ? new byte[0]
        : event.data().asSlice(0, event.dataLen()).toArray(ValueLayout.JAVA_BYTE);
  }
}
