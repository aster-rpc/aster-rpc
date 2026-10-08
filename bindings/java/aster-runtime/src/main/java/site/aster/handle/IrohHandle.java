package site.aster.handle;

import java.lang.ref.Cleaner;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.LongConsumer;

/**
 * Base for Aster native handles. Explicit close attempts native release exactly once.
 *
 * <p>Subclasses with synchronous cleanup may supply a detached action to the two-argument
 * constructor for a Cleaner backstop. The action must not capture this handle (directly or
 * indirectly). The original one-argument constructor requires explicit close; asynchronous
 * resources cannot be released by the Cleaner.
 */
public abstract class IrohHandle implements AutoCloseable {
  private static final Cleaner CLEANER = Cleaner.create();
  protected final long handle;
  private final AtomicBoolean closed = new AtomicBoolean();
  private final Cleaner.Cleanable cleanable;

  protected IrohHandle(long handle) {
    this(handle, null);
  }

  protected IrohHandle(long handle, LongConsumer detachedCleanup) {
    this.handle = handle;
    cleanable =
        detachedCleanup == null
            ? null
            : CLEANER.register(this, new Cleanup(handle, closed, detachedCleanup));
  }

  /** FFI function name identifying this native handle type. */
  protected abstract String freeNativeKind();

  /** Explicit native release. Cleaner cleanup uses the separately supplied detached action. */
  protected abstract void freeNative(long handle);

  @Override
  public void close() {
    if (claimClose()) {
      try {
        freeNative(handle);
      } finally {
        if (cleanable != null) cleanable.clean();
      }
    }
  }

  public long nativeHandle() {
    return handle;
  }

  /** Claim the first explicit close attempt for asynchronous subclasses. */
  protected final boolean claimClose() {
    return closed.compareAndSet(false, true);
  }

  public boolean isClosed() {
    return closed.get();
  }

  protected void checkNotClosed() {
    if (closed.get()) throw new IllegalStateException("handle already closed");
  }

  private record Cleanup(long handle, AtomicBoolean closed, LongConsumer release)
      implements Runnable {
    public void run() {
      if (closed.compareAndSet(false, true)) {
        try {
          release.accept(handle);
        } catch (Throwable error) {
          System.err.println("IrohHandle cleanup failed: " + error.getMessage());
        }
      }
    }
  }
}
