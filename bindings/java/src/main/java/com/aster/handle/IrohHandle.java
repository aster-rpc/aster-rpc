package com.aster.handle;

import java.lang.ref.Cleaner;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Base for all Aster handle types.
 *
 * <p>Wraps a native {@code uint64_t} handle. Subclasses must implement {@link #freeNative(long)} to
 * call the correct typed FFI free function. The {@link Cleaner} is used as a backstop if {@link
 * #close()} is not called before the Java object is GC'd.
 */
public abstract class IrohHandle implements AutoCloseable {

  private static final AtomicLong nextId = new AtomicLong(1);
  private static final Cleaner CLEANER = Cleaner.create();

  protected final long handle;
  private final long resourceId;
  private volatile boolean closed = false;

  protected IrohHandle(long handle) {
    this.handle = handle;
    this.resourceId = nextId.incrementAndGet();
    CLEANER.register(this, new Cleanup(handle, freeNativeKind()));
  }

  /** FFI function name to free this handle type (e.g. {@code "iroh_endpoint_free"}). */
  protected abstract String freeNativeKind();

  /** Called by the Cleaner (or explicitly) to free the native handle. */
  protected abstract void freeNative(long handle);

  @Override
  public void close() {
    if (!closed) {
      closed = true;
      freeNative(handle);
    }
  }

  public long nativeHandle() {
    return handle;
  }

  public boolean isClosed() {
    return closed;
  }

  protected void checkNotClosed() {
    if (closed) {
      throw new IllegalStateException("handle already closed");
    }
  }

  private record Cleanup(long handle, String kind) implements Runnable {
    public void run() {
      System.err.println("IrohHandle: " + kind + " handle " + handle + " freed by Cleaner");
    }
  }
}
