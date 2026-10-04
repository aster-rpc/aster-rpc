package site.aster.lease;

import java.lang.foreign.FunctionDescriptor;
import java.lang.invoke.MethodHandle;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.UpcallNative;

/**
 * A held lease ({@code aster_lease_t}, C ABI 4.1): fence access gated by the holder's self-limit,
 * background renewals at the serializer, and a status watch. {@link #close()} stops renewing
 * without releasing (the lease lapses by TTL); {@link #release()} hands it off cleanly.
 */
public final class Lease implements AutoCloseable {

  private static final MethodHandle FENCE =
      UpcallNative.handle(
          "aster_lease_fence",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.A));
  private static final MethodHandle STATUS =
      UpcallNative.handle(
          "aster_lease_status",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.A));
  private static final MethodHandle WATCH =
      UpcallNative.handle(
          "aster_lease_watch",
          FunctionDescriptor.of(
              UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.J, UpcallNative.A));
  private static final MethodHandle RELEASE =
      UpcallNative.handle(
          "aster_lease_release",
          FunctionDescriptor.of(
              UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.J, UpcallNative.A));
  private static final MethodHandle FREE =
      UpcallNative.handle(
          "aster_lease_free",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J));

  private final IrohRuntime runtime;
  private final long handle;
  private final Fence granted;
  private final AtomicBoolean done = new AtomicBoolean();

  Lease(IrohRuntime runtime, long handle, Fence granted) {
    this.runtime = runtime;
    this.handle = handle;
    this.granted = granted;
  }

  public long nativeHandle() {
    return handle;
  }

  /** The fence issued at acquire (resource, epoch, holder). */
  public Fence grantedFence() {
    return granted;
  }

  /**
   * The fence for a protected action, read at send time (do not cache it); empty once the holder
   * must stop acting (fenced, lost, released, or past its self-limit).
   */
  public Optional<Fence> fence() {
    if (done.get()) {
      return Optional.empty();
    }
    long epoch;
    try {
      epoch =
          UpcallNative.callForLong(
              "aster_lease_fence",
              (a, out) -> (int) FENCE.invoke(runtime.nativeHandle(), handle, out));
    } catch (IrohException e) {
      if (e.status() == IrohStatus.ALREADY_CLOSED || e.status() == IrohStatus.NOT_FOUND) {
        return Optional.empty();
      }
      throw e;
    }
    return Optional.of(new Fence(granted.resource(), epoch, granted.holder()));
  }

  public LeaseStatus status() {
    if (done.get()) {
      return LeaseStatus.RELEASED;
    }
    long code =
        UpcallNative.callForLong(
            "aster_lease_status",
            (a, out) -> {
              out.set(UpcallNative.J, 0, 0L);
              return (int) STATUS.invoke(runtime.nativeHandle(), handle, out);
            });
    return LeaseStatus.fromCode((int) code);
  }

  /** Completes when the lease leaves {@code HELD} (fenced, lost or released). */
  public CompletableFuture<LeaseStatus> watch() {
    return UpcallNative.submit(
            runtime,
            "aster_lease_watch",
            (a, op) -> (int) WATCH.invoke(runtime.nativeHandle(), handle, 0L, op))
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.LEASE_STATUS) {
                throw new IrohException("unexpected event " + r.event().kind());
              }
              if (r.event().status() != 0) {
                return LeaseStatus.RELEASED;
              }
              return LeaseStatus.fromCode(r.event().flags());
            });
  }

  /**
   * Stop renewing and RELEASE at the serializer (advances the fence; this holder's in-flight writes
   * are rejected from here on).
   */
  public CompletableFuture<Void> release() {
    if (!done.compareAndSet(false, true)) {
      return CompletableFuture.completedFuture(null);
    }
    return UpcallNative.submit(
            runtime,
            "aster_lease_release",
            (a, op) -> (int) RELEASE.invoke(runtime.nativeHandle(), handle, 0L, op))
        .thenApply(r -> null);
  }

  /** Stop renewing without releasing (the lease lapses at the serializer by TTL). */
  @Override
  public void close() {
    if (done.compareAndSet(false, true)) {
      try {
        FREE.invoke(runtime.nativeHandle(), handle);
      } catch (Throwable ignored) {
        // runtime closed
      }
    }
  }
}
