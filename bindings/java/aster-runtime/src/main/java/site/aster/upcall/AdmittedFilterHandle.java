package site.aster.upcall;

import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.invoke.MethodHandle;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohLibrary;
import site.aster.handle.IrohRuntime;

/**
 * The Rust side of a Java {@link AdmittedFilter} ({@code aster_topology_admitted_t}, C ABI 4.1): a
 * cache answered by batched {@code TOPOLOGY_ADMITTED} upcalls. Lookups never block: fresh answers
 * are served, stale ones served and refreshed, misses get the kind's failure-mode answer (default:
 * not admitted) and are resolved in the background. Install it with {@link Topology#enableShared}.
 */
public final class AdmittedFilterHandle implements AutoCloseable {

  /** Counters of the filter. */
  public record Stats(long hits, long stale, long misses, long upcalls) {}

  private static final MethodHandle NEW =
      UpcallNative.handle(
          "aster_topology_admitted_new",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              UpcallNative.I,
              UpcallNative.I,
              UpcallNative.I,
              UpcallNative.A));
  private static final MethodHandle FREE =
      UpcallNative.handle(
          "aster_topology_admitted_free",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J));
  private static final MethodHandle CHECK =
      UpcallNative.handle(
          "aster_topology_admitted_check",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              IrohLibrary.ASTER_BYTES,
              UpcallNative.A));
  private static final MethodHandle SET =
      UpcallNative.handle(
          "aster_topology_admitted_set",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              IrohLibrary.ASTER_BYTES,
              UpcallNative.I));
  private static final MethodHandle INVALIDATE =
      UpcallNative.handle(
          "aster_topology_admitted_invalidate",
          FunctionDescriptor.of(
              UpcallNative.I, UpcallNative.J, UpcallNative.J, IrohLibrary.ASTER_BYTES));
  private static final MethodHandle PREFETCH =
      UpcallNative.handle(
          "aster_topology_admitted_prefetch",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              IrohLibrary.ASTER_BYTES_LIST,
              UpcallNative.J,
              UpcallNative.A));
  private static final MethodHandle STATS =
      UpcallNative.handle(
          "aster_topology_admitted_stats",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.A));

  private final IrohRuntime runtime;
  private final long handle;
  private final AtomicBoolean closed = new AtomicBoolean();

  private AdmittedFilterHandle(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
  }

  /** Defaults: answers served for 30 s, failed batches retried after 1 s, 256 ids per upcall. */
  public static AdmittedFilterHandle create(Upcalls upcalls) {
    return create(upcalls, Duration.ZERO, Duration.ZERO, 0);
  }

  /** Zero values take the defaults. */
  public static AdmittedFilterHandle create(
      Upcalls upcalls, Duration ttl, Duration failureRetry, int maxBatch) {
    IrohRuntime rt = upcalls.runtime();
    long h =
        UpcallNative.callForLong(
            "aster_topology_admitted_new",
            (a, out) ->
                (int)
                    NEW.invoke(
                        rt.nativeHandle(),
                        upcalls.nativeHandle(),
                        (int) ttl.toMillis(),
                        (int) failureRetry.toMillis(),
                        maxBatch,
                        out));
    return new AdmittedFilterHandle(rt, h);
  }

  public long nativeHandle() {
    return handle;
  }

  /** Look {@code nodeId} up exactly as the swarm does (a miss queues a resolution). */
  public boolean isAdmitted(String nodeId) {
    long v =
        UpcallNative.callForLong(
            "aster_topology_admitted_check",
            (a, out) -> {
              out.set(UpcallNative.J, 0, 0L);
              return (int)
                  CHECK.invoke(runtime.nativeHandle(), handle, DataPlane.utf8(a, nodeId), out);
            });
    return (v & 0xffff_ffffL) != 0;
  }

  /** Push an answer (e.g. on revocation); it takes effect at the next derived view. */
  public void set(String nodeId, boolean admitted) {
    UpcallNative.callForLong(
        "aster_topology_admitted_set",
        (a, out) ->
            (int)
                SET.invoke(
                    runtime.nativeHandle(), handle, DataPlane.utf8(a, nodeId), admitted ? 1 : 0));
  }

  /** Forget one id's answer. */
  public void invalidate(String nodeId) {
    UpcallNative.callForLong(
        "aster_topology_admitted_invalidate",
        (a, out) ->
            (int) INVALIDATE.invoke(runtime.nativeHandle(), handle, DataPlane.utf8(a, nodeId)));
  }

  /** Forget every answer. */
  public void clear() {
    invalidate("");
  }

  /** Resolve {@code nodeIds} now with one upcall (warm the cache before joining a swarm). */
  public CompletableFuture<Void> prefetch(List<String> nodeIds) {
    return UpcallNative.submit(
            runtime,
            "aster_topology_admitted_prefetch",
            (a, op) ->
                (int)
                    PREFETCH.invoke(
                        runtime.nativeHandle(), handle, DataPlane.utf8List(a, nodeIds), 0L, op))
        .thenApply(r -> null);
  }

  public Stats stats() {
    try (var arena = java.lang.foreign.Arena.ofConfined()) {
      MemorySegment out = arena.allocate(32, 8);
      UpcallNative.check(
          (int) STATS.invoke(runtime.nativeHandle(), handle, out), "aster_topology_admitted_stats");
      return new Stats(
          out.get(UpcallNative.J, 0),
          out.get(UpcallNative.J, 8),
          out.get(UpcallNative.J, 16),
          out.get(UpcallNative.J, 24));
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

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
