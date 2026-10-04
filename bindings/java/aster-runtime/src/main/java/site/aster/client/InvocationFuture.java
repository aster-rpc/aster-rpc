package site.aster.client;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicReference;
import site.aster.ffi.Rpc;

/**
 * The future of an in-flight call. Cancelling it ({@link #cancel}) cancels the native call the call
 * is currently running on ({@code aster_call_cancel}: {@code FLAG_CANCEL} plus a stream reset), so
 * the server sees the cancel; a plain {@link CompletableFuture} could only abandon the result.
 */
final class InvocationFuture<T> extends CompletableFuture<T> {

  private final AtomicReference<Runnable> onCancel = new AtomicReference<>();

  /**
   * Register what cancelling does now (replacing any earlier hook); runs it at once if the future
   * was already cancelled. {@code hook} must be idempotent.
   */
  void onCancel(Runnable hook) {
    onCancel.set(hook);
    if (isCancelled()) {
      hook.run();
    }
  }

  void clearOnCancel() {
    onCancel.set(null);
  }

  @Override
  public boolean cancel(boolean mayInterruptIfRunning) {
    boolean cancelled = super.cancel(mayInterruptIfRunning);
    if (cancelled) {
      Runnable hook = onCancel.get();
      if (hook != null) {
        hook.run();
      }
    }
    return cancelled;
  }

  /**
   * A native call handle that is safe to cancel from another thread while its owner reads from it,
   * and never cancelled once released.
   */
  static final class NativeCall {
    private final long runtime;
    private final long call;
    private boolean released;

    NativeCall(long runtime, long call) {
      this.runtime = runtime;
      this.call = call;
    }

    long runtime() {
      return runtime;
    }

    long handle() {
      return call;
    }

    /** Idempotent. */
    synchronized void cancel() {
      if (!released) {
        Rpc.cancel(runtime, call);
      }
    }

    /** Idempotent: frees the call (cancelling it if unfinished). */
    synchronized void release() {
      if (!released) {
        released = true;
        Rpc.release(runtime, call);
      }
    }
  }
}
