package site.aster.coordination;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.coordination.Coordination.Fence;
import site.aster.coordination.Coordination.MutationReply;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;
import site.aster.lease.LeaseStatus;

/**
 * A held coordination lease ({@code aster_coordination_lease_t}): the Rust renew loop and
 * self-limit run for it, and every transition is journaled and acknowledged by the lease itself.
 * Ask {@link #fence()} immediately before each protected action and never cache the result.
 *
 * <p><b>Release is not prompt.</b> {@link #release}, {@link #releaseWithValue} and a client close
 * wait for an in-flight {@link #compareAndSet} on this lease, which is one journaled mutation the
 * driver may retry for its full budget (8 attempts with backoff) before it resolves or fails; the
 * journal lifetime lock stays held until it ends. Do not model them as immediate; {@link #fence}
 * and {@link #status} never block (while a release is pending they report "stop" / {@code
 * RELEASED}). {@link #close()} frees the handle <b>without</b> releasing: renewals stop and the
 * lease lapses by TTL.
 */
public final class CoordinationLease implements AutoCloseable {

  private final IrohRuntime runtime;
  private final long handle;
  private final Fence grantedFence;
  private final AtomicBoolean consumed = new AtomicBoolean();

  CoordinationLease(IrohRuntime runtime, long handle, Fence grantedFence) {
    this.runtime = runtime;
    this.handle = handle;
    this.grantedFence = grantedFence;
  }

  public long nativeHandle() {
    return handle;
  }

  /** The fence the acquire was granted with (a snapshot; use {@link #fence()} to act). */
  public Fence grantedFence() {
    return grantedFence;
  }

  /**
   * The current full fence for a protected action, or empty once the holder must stop (not held,
   * the self-limit passed, released, being released, or the client closed). Never blocks.
   */
  public Optional<Fence> fence() {
    if (consumed.get()) {
      return Optional.empty();
    }
    try (Arena a = Arena.ofConfined()) {
      MemorySegment len = a.allocate(ValueLayout.JAVA_LONG);
      int rc =
          (int)
              CoordNative.LEASE_FENCE.invoke(
                  runtime.nativeHandle(), handle, MemorySegment.NULL, 0L, len);
      if (rc == IrohStatus.ALREADY_CLOSED.code) {
        return Optional.empty();
      }
      if (rc != IrohStatus.BUFFER_TOO_SMALL.code) {
        CoordNative.check(rc, "aster_coordination_lease_fence");
        return Optional.empty();
      }
      long need = len.get(ValueLayout.JAVA_LONG, 0);
      MemorySegment buf = a.allocate(need);
      rc = (int) CoordNative.LEASE_FENCE.invoke(runtime.nativeHandle(), handle, buf, need, len);
      if (rc == IrohStatus.ALREADY_CLOSED.code) {
        return Optional.empty();
      }
      CoordNative.check(rc, "aster_coordination_lease_fence");
      byte[] d = buf.asSlice(0, len.get(ValueLayout.JAVA_LONG, 0)).toArray(ValueLayout.JAVA_BYTE);
      return Optional.of(CoordinationCodec.decodeFence(d));
    } catch (Throwable t) {
      throw CoordNative.rethrow(t);
    }
  }

  /** The holder-side status; {@code RELEASED} once released, being released, or client closed. */
  public LeaseStatus status() {
    if (consumed.get()) {
      return LeaseStatus.RELEASED;
    }
    try (Arena a = Arena.ofConfined()) {
      MemorySegment out = a.allocate(ValueLayout.JAVA_INT);
      CoordNative.check(
          (int) CoordNative.LEASE_STATUS.invoke(runtime.nativeHandle(), handle, out),
          "aster_coordination_lease_status");
      int code = out.get(ValueLayout.JAVA_INT, 0);
      return LeaseStatus.values()[Math.min(Math.max(code, 0), LeaseStatus.values().length - 1)];
    } catch (Throwable t) {
      throw CoordNative.rethrow(t);
    }
  }

  /**
   * {@code compare_and_set} on the register under the lease's current fence; journaled and
   * acknowledged by the lease. The reply carries the register's new state or a typed rejection.
   * {@code CLOSED} when the lease is closed or being released.
   */
  public CompletableFuture<MutationReply> compareAndSet(
      long expectedValueRevision, byte[] newValue) {
    return CoordNative.submit(
        runtime,
        "aster_coordination_lease_compare_and_set",
        (a, out) ->
            (int)
                CoordNative.LEASE_CAS.invoke(
                    runtime.nativeHandle(),
                    handle,
                    expectedValueRevision,
                    DataPlane.bytes(a, newValue),
                    0L,
                    out),
        r -> CoordinationCodec.decodeMutationReply(r.data()));
  }

  /**
   * Graceful release through the lease's own transition. <b>Consumes this handle</b>; waits for an
   * in-flight {@link #compareAndSet}.
   */
  public CompletableFuture<Void> release() {
    if (!consumed.compareAndSet(false, true)) {
      return CompletableFuture.failedFuture(
          CoordinationExceptions.notFound("lease already released or freed"));
    }
    return CoordNative.submit(
        runtime,
        "aster_coordination_lease_release",
        (a, out) -> (int) CoordNative.LEASE_RELEASE.invoke(runtime.nativeHandle(), handle, 0L, out),
        r -> null);
  }

  /**
   * The atomic final handoff: write {@code finalValue} and release together, or neither. Consumes
   * this handle; waits for an in-flight {@link #compareAndSet}.
   */
  public CompletableFuture<MutationReply> releaseWithValue(
      long expectedValueRevision, byte[] finalValue) {
    if (!consumed.compareAndSet(false, true)) {
      return CompletableFuture.failedFuture(
          CoordinationExceptions.notFound("lease already released or freed"));
    }
    return CoordNative.submit(
        runtime,
        "aster_coordination_lease_release_with_value",
        (a, out) ->
            (int)
                CoordNative.LEASE_RELEASE_WITH_VALUE.invoke(
                    runtime.nativeHandle(),
                    handle,
                    expectedValueRevision,
                    DataPlane.bytes(a, finalValue),
                    0L,
                    out),
        r -> CoordinationCodec.decodeMutationReply(r.data()));
  }

  /** Free the handle without releasing (once); the lease lapses by TTL. */
  @Override
  public void close() {
    if (consumed.compareAndSet(false, true)) {
      try {
        CoordNative.LEASE_FREE.invoke(runtime.nativeHandle(), handle);
      } catch (Throwable ignored) {
        // runtime already closed
      }
    }
  }
}
