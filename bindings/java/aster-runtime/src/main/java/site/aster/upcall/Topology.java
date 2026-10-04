package site.aster.upcall;

import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.invoke.MethodHandle;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import site.aster.ffi.IrohLibrary;
import site.aster.node.IrohNode;

/** Topology v2 swarm membership with an optional Java admission filter (C ABI 4.1). */
public final class Topology {

  private Topology() {}

  private static final MethodHandle ENABLE_SHARED =
      UpcallNative.handle(
          "aster_topology_enable_shared",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              UpcallNative.A,
              UpcallNative.J,
              IrohLibrary.ASTER_BYTES_LIST,
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.A));

  /**
   * Join the topology swarm keyed by the 32-byte namespace {@code secret}: publish this node's
   * measurements and derive the shared cluster view, counting only authors {@code filter} admits
   * (null = every holder of the secret). The node must run with monitoring enabled; {@code filter}
   * must belong to the node's runtime. {@code positionRefresh} zero = 60 s.
   */
  public static CompletableFuture<Void> enableShared(
      IrohNode node,
      byte[] secret,
      AdmittedFilterHandle filter,
      List<String> bootstrap,
      Duration positionRefresh) {
    if (secret == null || secret.length != 32) {
      throw new IllegalArgumentException("secret must be 32 bytes");
    }
    var rt = node.runtime();
    return UpcallNative.submit(
            rt,
            "aster_topology_enable_shared",
            (a, op) -> {
              MemorySegment key = a.allocate(32);
              key.copyFrom(MemorySegment.ofArray(secret));
              return (int)
                  ENABLE_SHARED.invoke(
                      rt.nativeHandle(),
                      node.nodeHandle(),
                      key,
                      filter == null ? 0L : filter.nativeHandle(),
                      site.aster.ffi.DataPlane.utf8List(a, bootstrap),
                      (int) positionRefresh.toMillis(),
                      0L,
                      op);
            })
        .thenApply(r -> null);
  }
}
