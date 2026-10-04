package site.aster.upcall;

import java.util.List;

/**
 * The topology v2 read-time admission filter, served by batched upcall (kind {@code
 * TOPOLOGY_ADMITTED}): one answer per node id (64 lowercase hex), same order. Rust caches the
 * answers, so this is called per batch of newly seen or stale ids, not per read.
 */
@FunctionalInterface
public interface AdmittedFilter {
  List<Boolean> admitted(List<String> nodeIds) throws Exception;
}
