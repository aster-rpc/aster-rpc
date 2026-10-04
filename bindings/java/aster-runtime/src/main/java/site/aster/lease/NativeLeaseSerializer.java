package site.aster.lease;

import java.lang.foreign.FunctionDescriptor;
import java.lang.invoke.MethodHandle;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.Ucv1;
import site.aster.upcall.UpcallNative;
import site.aster.upcall.Upcalls;

/**
 * A Rust-side lease serializer ({@code aster_lease_serializer_t}, C ABI 4.1) that lease handles are
 * acquired from: either {@link #foreign} (the serialization point is a Java {@link LeaseSerializer}
 * served on an {@link Upcalls} table) or {@link #memory} (Rust's reference in-memory serializer,
 * optionally guarded by a Java {@link LeaseAuthorizer}).
 */
public final class NativeLeaseSerializer implements AutoCloseable {

  private static final MethodHandle FOREIGN_NEW =
      UpcallNative.handle(
          "aster_lease_serializer_foreign_new",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.A));
  private static final MethodHandle MEMORY_NEW =
      UpcallNative.handle(
          "aster_lease_serializer_memory_new",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.A));
  private static final MethodHandle FREE =
      UpcallNative.handle(
          "aster_lease_serializer_free",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J));
  private static final MethodHandle ACQUIRE =
      UpcallNative.handle(
          "aster_lease_acquire",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              site.aster.ffi.IrohLibrary.ASTER_BYTES,
              site.aster.ffi.IrohLibrary.ASTER_BYTES,
              UpcallNative.J,
              UpcallNative.J,
              UpcallNative.J,
              UpcallNative.J,
              UpcallNative.A));
  private static final MethodHandle SNAPSHOT =
      UpcallNative.handle(
          "aster_lease_snapshot",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              site.aster.ffi.IrohLibrary.ASTER_BYTES,
              UpcallNative.J,
              UpcallNative.A));
  private static final MethodHandle REVOKE =
      UpcallNative.handle(
          "aster_lease_revoke",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              site.aster.ffi.IrohLibrary.ASTER_BYTES,
              site.aster.ffi.IrohLibrary.ASTER_BYTES,
              UpcallNative.J,
              UpcallNative.A));

  private final IrohRuntime runtime;
  private final long handle;
  private final AtomicBoolean closed = new AtomicBoolean();

  private NativeLeaseSerializer(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
  }

  /**
   * A serializer whose transitions and snapshots are upcalls on {@code upcalls} (register the Java
   * implementation with {@link Upcalls#serveLeaseSerializer}). A failed upcall is a backend error,
   * never a denial.
   */
  public static NativeLeaseSerializer foreign(Upcalls upcalls) {
    IrohRuntime rt = upcalls.runtime();
    long h =
        UpcallNative.callForLong(
            "aster_lease_serializer_foreign_new",
            (a, out) -> (int) FOREIGN_NEW.invoke(rt.nativeHandle(), upcalls.nativeHandle(), out));
    return new NativeLeaseSerializer(rt, h);
  }

  /**
   * Rust's in-memory reference serializer. With {@code authorizer} non-null every transition is
   * first put to it as a {@code LEASE_AUTHORIZE} decision (register the Java authorizer with {@link
   * Upcalls#serveLeaseAuthorizer}).
   */
  public static NativeLeaseSerializer memory(IrohRuntime runtime, Upcalls authorizer) {
    long u = authorizer == null ? 0 : authorizer.nativeHandle();
    long h =
        UpcallNative.callForLong(
            "aster_lease_serializer_memory_new",
            (a, out) -> (int) MEMORY_NEW.invoke(runtime.nativeHandle(), u, out));
    return new NativeLeaseSerializer(runtime, h);
  }

  public long nativeHandle() {
    return handle;
  }

  /** Acquire with the default timing (30 s ttl, 10 s renew, 1 s holder margin). */
  public CompletableFuture<AcquireResult> acquire(String resource, String holder) {
    return acquire(resource, holder, 0, 0, 0);
  }

  /**
   * One ACQUIRE at the serializer. {@code ttlMs} 0 = defaults; with a ttl, 0 for {@code renewMs} =
   * ttl/3 and 0 for {@code marginMs} = ttl/10 (at most 1 s).
   */
  public CompletableFuture<AcquireResult> acquire(
      String resource, String holder, long ttlMs, long renewMs, long marginMs) {
    return UpcallNative.submit(
            runtime,
            "aster_lease_acquire",
            (a, op) ->
                (int)
                    ACQUIRE.invoke(
                        runtime.nativeHandle(),
                        handle,
                        DataPlane.utf8(a, resource),
                        DataPlane.utf8(a, holder),
                        ttlMs,
                        renewMs,
                        marginMs,
                        0L,
                        op))
        .thenApply(r -> toAcquireResult(r));
  }

  private AcquireResult toAcquireResult(DataPlane.Result r) {
    if (r.event().kind() != IrohEventKind.LEASE_ACQUIRED) {
      throw new IrohException("unexpected event " + r.event().kind());
    }
    TransitionOutcome o = Ucv1.decodeTransitionOutcome(r.data());
    return switch (o) {
      case TransitionOutcome.Granted g ->
          new AcquireResult.Granted(new Lease(runtime, r.event().handle(), g.fence()));
      case TransitionOutcome.Denied d -> new AcquireResult.Denied(d.reason(), d.snapshot());
      default -> throw new IrohException("serializer answered " + o + " to an acquire");
    };
  }

  /** Advisory snapshot of {@code resource}. */
  public CompletableFuture<LeaseSnapshot> snapshot(String resource) {
    return UpcallNative.submit(
            runtime,
            "aster_lease_snapshot",
            (a, op) ->
                (int)
                    SNAPSHOT.invoke(
                        runtime.nativeHandle(), handle, DataPlane.utf8(a, resource), 0L, op))
        .thenApply(r -> Ucv1.decodeLeaseSnapshot(r.data()));
  }

  /** REVOKE {@code resource} on behalf of {@code authority} (advances the fence). */
  public CompletableFuture<TransitionOutcome> revoke(String resource, String authority) {
    return UpcallNative.submit(
            runtime,
            "aster_lease_revoke",
            (a, op) ->
                (int)
                    REVOKE.invoke(
                        runtime.nativeHandle(),
                        handle,
                        DataPlane.utf8(a, resource),
                        DataPlane.utf8(a, authority),
                        0L,
                        op))
        .thenApply(r -> Ucv1.decodeTransitionOutcome(r.data()));
  }

  /** Free the handle; leases acquired from it keep renewing. */
  @Override
  public void close() {
    if (closed.compareAndSet(false, true)) {
      try {
        FREE.invoke(runtime.nativeHandle(), handle);
      } catch (Throwable ignored) {
        // runtime closed
      }
    }
  }
}
