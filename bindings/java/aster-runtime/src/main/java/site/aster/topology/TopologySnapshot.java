package site.aster.topology;

import java.lang.foreign.Arena;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import site.aster.ffi.IrohStatus;
import site.aster.ffi.NativeCall;
import site.aster.handle.IrohRuntime;

/** Owned immutable native shared view. Queries and close are serialized; close is idempotent. */
public final class TopologySnapshot implements AutoCloseable {
  private static final ValueLayout.OfLong J = ValueLayout.JAVA_LONG;
  private static final java.lang.foreign.AddressLayout A = ValueLayout.ADDRESS;
  private static final MethodHandle FREE = Topology.handle("aster_topology_snapshot_free", J, J);
  private static final MethodHandle CLUSTERS =
      Topology.handle("aster_topology_snapshot_clusters", J, J, A);
  private static final MethodHandle CLUSTER =
      Topology.handle("aster_topology_snapshot_cluster_of", J, J, NativeCall.BYTES, A);
  private static final MethodHandle SEPARATED =
      Topology.handle(
          "aster_topology_snapshot_separated", J, J, NativeCall.BYTES, NativeCall.BYTES, A);
  private final IrohRuntime runtime;
  private final long handle;
  private boolean closed;

  TopologySnapshot(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
  }

  public synchronized List<Topology.ClusterView> clusters() {
    checkOpen();
    var values =
        Topology.json(CLUSTERS, "topology_snapshot_clusters", runtime.nativeHandle(), handle);
    var result = new ArrayList<Topology.ClusterView>();
    values.forEach(v -> result.add(Topology.cluster(v)));
    return List.copyOf(result);
  }

  public synchronized Optional<Topology.ClusterView> clusterOf(String nodeId) {
    checkOpen();
    try (Arena a = Arena.ofConfined()) {
      var value =
          Topology.json(
              CLUSTER,
              "topology_snapshot_cluster_of",
              runtime.nativeHandle(),
              handle,
              NativeCall.string(a, nodeId));
      return value.isNull() ? Optional.empty() : Optional.of(Topology.cluster(value));
    }
  }

  /** Native coverage-based verdict from this same view; UNKNOWN is not separation. */
  public synchronized Topology.SeparationVerdict separated(String first, String second) {
    checkOpen();
    try (Arena a = Arena.ofConfined()) {
      var out = a.allocate(ValueLayout.JAVA_INT);
      NativeCall.callChecked(
          SEPARATED,
          "topology_snapshot_separated",
          runtime.nativeHandle(),
          handle,
          NativeCall.string(a, first),
          NativeCall.string(a, second),
          out);
      int value = out.get(ValueLayout.JAVA_INT, 0);
      if (value < 0 || value >= Topology.SeparationVerdict.values().length)
        throw new IllegalStateException("invalid native separation verdict");
      return Topology.SeparationVerdict.values()[value];
    }
  }

  private void checkOpen() {
    if (closed) throw new IllegalStateException("topology snapshot closed");
  }

  @Override
  public synchronized void close() {
    if (closed) return;
    int rc = NativeCall.call(FREE, runtime.nativeHandle(), handle);
    if (rc != 0 && rc != IrohStatus.NOT_FOUND.code && rc != IrohStatus.ALREADY_CLOSED.code)
      NativeCall.check(rc, "topology_snapshot_free");
    closed = true;
  }
}
