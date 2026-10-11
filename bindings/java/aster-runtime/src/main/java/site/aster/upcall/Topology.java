package site.aster.upcall;

import java.time.Duration;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import site.aster.node.IrohNode;

/**
 * Compatibility entry point for shared topology. Full views are available via IrohNode.topology().
 */
public final class Topology {
  private Topology() {}

  /** Join with native timing defaults and an optional runtime-owned read-time admission filter. */
  public static CompletableFuture<Void> enableShared(
      IrohNode node,
      byte[] secret,
      AdmittedFilterHandle filter,
      List<String> bootstrap,
      Duration positionRefresh) {
    return node.topology()
        .enableSharedAsync(
            secret,
            new site.aster.topology.Topology.SwarmOptions(
                bootstrap, filter, positionRefresh, null));
  }
}
