package site.aster.coordination;

import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.function.Function;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.UpcallNative;

/** Method handles of the ABI 4.4 coordination functions ({@code coordination.rs}). */
final class CoordNative {

  private CoordNative() {}

  private static final ValueLayout.OfInt I = UpcallNative.I;
  private static final ValueLayout.OfLong J = UpcallNative.J;
  private static final java.lang.foreign.AddressLayout A = UpcallNative.A;
  private static final MemoryLayout B = IrohLibrary.ASTER_BYTES;

  /** {@code ASTER_COORDINATION_CLIENT_READ_ONLY}. */
  static final int FLAG_READ_ONLY = 1;

  /** aster_coordination_client_options_t: 48 bytes (LP64). */
  static final MemoryLayout OPTIONS_LAYOUT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), //      0
          ValueLayout.JAVA_INT.withName("flags"), //           4
          ValueLayout.ADDRESS.withName("group_id"), //         8
          IrohLibrary.ASTER_BYTES_LIST.withName("seed_tickets"), // 16
          IrohLibrary.ASTER_BYTES.withName("journal_dir")); // 32 -> 48

  static {
    long[] at = {0, 4, 8, 16, 32};
    String[] names = {"struct_size", "flags", "group_id", "seed_tickets", "journal_dir"};
    if (OPTIONS_LAYOUT.byteSize() != 48) {
      throw new AssertionError("aster_coordination_client_options_t size drift");
    }
    for (int i = 0; i < names.length; i++) {
      if (OPTIONS_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement(names[i])) != at[i]) {
        throw new AssertionError("aster_coordination_client_options_t offset drift: " + names[i]);
      }
    }
  }

  private static MethodHandle h(String name, FunctionDescriptor d) {
    return UpcallNative.handle(name, d);
  }

  static final MethodHandle OPTIONS_INIT =
      h("aster_coordination_client_options_init", FunctionDescriptor.of(I, A));
  static final MethodHandle CLIENT_OPEN =
      h("aster_coordination_client_open", FunctionDescriptor.of(I, J, J, A, J, A));
  static final MethodHandle CLIENT_CLOSE =
      h("aster_coordination_client_close", FunctionDescriptor.of(I, J, J));
  static final MethodHandle CLIENT_FREE =
      h("aster_coordination_client_free", FunctionDescriptor.of(I, J, J));
  static final MethodHandle CLIENT_IDS =
      h("aster_coordination_client_ids", FunctionDescriptor.of(I, J, J, A, A));
  static final MethodHandle CLIENT_ID =
      h("aster_coordination_client_id", FunctionDescriptor.of(I, J, J, J, A));
  static final MethodHandle MUTATE =
      h("aster_coordination_mutate", FunctionDescriptor.of(I, J, J, B, J, A));
  static final MethodHandle UNACKNOWLEDGED =
      h("aster_coordination_unacknowledged", FunctionDescriptor.of(I, J, J, J, A));
  static final MethodHandle RESUME =
      h("aster_coordination_resume", FunctionDescriptor.of(I, J, J, J, A));
  static final MethodHandle ACKNOWLEDGE =
      h("aster_coordination_resolved_acknowledge", FunctionDescriptor.of(I, J, J, J, A));
  static final MethodHandle RESOLVED_FREE =
      h("aster_coordination_resolved_free", FunctionDescriptor.of(I, J, J));
  static final MethodHandle RETIRE_OTHER =
      h("aster_coordination_retire_other_client", FunctionDescriptor.of(I, J, J, A, J, A));
  static final MethodHandle READ =
      h("aster_coordination_read", FunctionDescriptor.of(I, J, J, B, J, A));
  static final MethodHandle EXPORT_TO_FILE =
      h("aster_coordination_recovery_export_to_file", FunctionDescriptor.of(I, J, J, B, J, A));
  static final MethodHandle LEASE_ACQUIRE =
      h("aster_coordination_lease_acquire", FunctionDescriptor.of(I, J, J, B, B, J, J, J, J, A));
  static final MethodHandle LEASE_FENCE =
      h("aster_coordination_lease_fence", FunctionDescriptor.of(I, J, J, A, J, A));
  static final MethodHandle LEASE_STATUS =
      h("aster_coordination_lease_status", FunctionDescriptor.of(I, J, J, A));
  static final MethodHandle LEASE_CAS =
      h("aster_coordination_lease_compare_and_set", FunctionDescriptor.of(I, J, J, J, B, J, A));
  static final MethodHandle LEASE_RELEASE =
      h("aster_coordination_lease_release", FunctionDescriptor.of(I, J, J, J, A));
  static final MethodHandle LEASE_RELEASE_WITH_VALUE =
      h("aster_coordination_lease_release_with_value", FunctionDescriptor.of(I, J, J, J, B, J, A));
  static final MethodHandle LEASE_FREE =
      h("aster_coordination_lease_free", FunctionDescriptor.of(I, J, J));
  static final MethodHandle MANAGER_NEW =
      h("aster_coordination_placement_manager_new", FunctionDescriptor.of(I, J, J, J, J, A));
  static final MethodHandle RECONCILE =
      h("aster_coordination_placement_reconcile", FunctionDescriptor.of(I, J, J, J, A));
  static final MethodHandle MANAGER_FREE =
      h("aster_coordination_placement_manager_free", FunctionDescriptor.of(I, J, J));

  /** A synchronous non-OK status as a {@link CoordinationException}. */
  static void check(int status, String name) {
    if (status != 0) {
      throw CoordinationException.from(
          new IrohException(
              IrohStatus.fromCode(status), name + " failed: " + DataPlane.lastError()));
    }
  }

  static RuntimeException rethrow(Throwable t) {
    if (t instanceof IrohException e) {
      return CoordinationException.from(e);
    }
    return UpcallNative.rethrow(t);
  }

  /** Submit an async call; native failures (event or status) surface as CoordinationException. */
  static <T> CompletableFuture<T> submit(
      IrohRuntime rt,
      String name,
      UpcallNative.OutCall call,
      Function<DataPlane.Result, T> result) {
    return UpcallNative.submit(rt, name, call)
        .handle(
            (r, t) -> {
              if (t != null) {
                Throwable c = t instanceof CompletionException ? t.getCause() : t;
                if (c instanceof IrohException e) {
                  throw CoordinationException.from(e);
                }
                if (c instanceof RuntimeException re) {
                  throw re;
                }
                throw new CompletionException(c);
              }
              return result.apply(r);
            });
  }

  static MemorySegment bytes(java.lang.foreign.SegmentAllocator a, byte[] data) {
    return DataPlane.bytes(a, data);
  }
}
