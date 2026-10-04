package site.aster.coordination;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.coordination.Coordination.PlacementReport;
import site.aster.coordination.Placement.PolicySource;
import site.aster.coordination.Placement.Provisioner;
import site.aster.handle.IrohRuntime;
import site.aster.node.IrohNode;
import site.aster.upcall.Ucv1;
import site.aster.upcall.UpcallKind;
import site.aster.upcall.UpcallNative;
import site.aster.upcall.Upcalls;

/**
 * The placement manager (design section 5) over a durable {@link CoordinationClient}: each {@link
 * #reconcile} pass asks the Java {@link PolicySource} (kind 32) for the desired roles, the topology
 * (kind 17, or the node's own shared view) and, for a candidate without seed material, the Java
 * {@link Provisioner} (kind 33); it changes membership only through the client's durable journal,
 * authorized by the replicated administrator predicate like any other mutation.
 *
 * <p><b>Providers never fail open.</b> An exception, timeout (the pass sees {@code CANCEL timeout};
 * configure the kind with {@code upcalls.setPolicy}), overload, a closed table or an undecodable
 * answer fails the pass; so does a provisioner answering for a different node. Passes on one
 * manager are serialized; closing the client retires the manager after any running pass.
 *
 * <p><b>One manager per {@link Upcalls} table.</b> A manager answers kinds 32, 33 and (without a
 * topology node) 17 on its table, one handler per kind: a second manager on the same table would
 * replace the first one's providers.
 */
public final class PlacementManager implements AutoCloseable {

  /** The topology a pass sees, when it does not come from a node's shared view. */
  @FunctionalInterface
  public interface TopologySource {
    Ucv1.TopologySnapshot snapshot() throws Exception;
  }

  private final IrohRuntime runtime;
  private final long handle;
  private final AtomicBoolean closed = new AtomicBoolean();

  private PlacementManager(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
  }

  /**
   * A manager whose topology is {@code topologyNode}'s own shared view (that node must have joined
   * a swarm with {@code Topology.enableShared}); the policy source and provisioner are answered on
   * {@code upcalls} (which must belong to the client's runtime). {@code provisioner} may be null if
   * every candidate carries its own seed (a candidate without one then fails the pass).
   */
  public static PlacementManager create(
      CoordinationClient client,
      Upcalls upcalls,
      PolicySource policy,
      Provisioner provisioner,
      IrohNode topologyNode) {
    return build(client, upcalls, policy, provisioner, null, topologyNode);
  }

  /** A manager whose topology is answered by {@code topology} on {@code upcalls} (kind 17). */
  public static PlacementManager create(
      CoordinationClient client,
      Upcalls upcalls,
      PolicySource policy,
      Provisioner provisioner,
      TopologySource topology) {
    if (topology == null) {
      throw new NullPointerException("topology");
    }
    return build(client, upcalls, policy, provisioner, topology, null);
  }

  private static PlacementManager build(
      CoordinationClient client,
      Upcalls upcalls,
      PolicySource policy,
      Provisioner provisioner,
      TopologySource topology,
      IrohNode topologyNode) {
    if (upcalls.runtime() != client.node().runtime()) {
      throw new IllegalArgumentException("upcalls must belong to the client's runtime");
    }
    if (policy == null) {
      throw new NullPointerException("policy");
    }
    IrohRuntime rt = upcalls.runtime();
    upcalls.handle(
        UpcallKind.PLACEMENT_POLICY,
        (req, dl) -> {
          Placement.decodePolicyRequest(req);
          Placement.PlacementPolicy p = policy.policy();
          if (p == null) {
            throw new IllegalStateException("PolicySource returned null");
          }
          return Placement.encodePolicy(p);
        });
    if (provisioner != null) {
      upcalls.handle(
          UpcallKind.PLACEMENT_PROVISION,
          (req, dl) ->
              CoordinationCodec.encodeMemberSeedMessage(
                  provisioner.provision(Placement.decodeProvisionRequest(req))));
    } else {
      upcalls.removeHandler(UpcallKind.PLACEMENT_PROVISION);
    }
    if (topology != null) {
      upcalls.handle(
          UpcallKind.TOPOLOGY_SNAPSHOT,
          (req, dl) -> {
            Ucv1.decodeTopologySnapshotRequest(req);
            return Ucv1.encodeTopologySnapshot(topology.snapshot());
          });
    }
    long h;
    try {
      h =
          UpcallNative.callForLong(
              "aster_coordination_placement_manager_new",
              (a, out) ->
                  (int)
                      CoordNative.MANAGER_NEW.invoke(
                          rt.nativeHandle(),
                          client.nativeHandle(),
                          upcalls.nativeHandle(),
                          topologyNode == null ? 0L : topologyNode.nodeHandle(),
                          out));
    } catch (site.aster.ffi.IrohException e) {
      throw CoordinationException.from(e);
    }
    return new PlacementManager(rt, h);
  }

  public long nativeHandle() {
    return handle;
  }

  /**
   * Run one reconciliation pass. Completes with the report (what was selected and, if membership
   * had to change, the submitted operation) or fails: with a provider's message when a provider
   * failed, {@code CLOSED} once the client is closed.
   */
  public CompletableFuture<PlacementReport> reconcile() {
    return CoordNative.submit(
        runtime,
        "aster_coordination_placement_reconcile",
        (a, out) -> (int) CoordNative.RECONCILE.invoke(runtime.nativeHandle(), handle, 0L, out),
        r -> CoordinationCodec.decodePlacementReport(r.data()));
  }

  /** Free the manager (once), after any running pass; the provider handlers stay on the table. */
  @Override
  public void close() {
    if (closed.compareAndSet(false, true)) {
      try {
        CoordNative.MANAGER_FREE.invoke(runtime.nativeHandle(), handle);
      } catch (Throwable ignored) {
        // runtime already closed
      }
    }
  }
}
