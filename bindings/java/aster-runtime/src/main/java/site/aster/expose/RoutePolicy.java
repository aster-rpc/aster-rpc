package site.aster.expose;

import java.util.List;

/**
 * An edge's admission hook for {@link Expose#serveRoutes} (upcall kind {@code
 * expose_route_policy}): returns the subset of the requested {@code specs} to grant. An exception,
 * timeout or a spec that was not requested is no usable answer, so the kind's failure mode applies
 * to the whole request: fail-closed (default) grants nothing.
 */
@FunctionalInterface
public interface RoutePolicy {

  List<RouteSpec> grant(String peerId, byte[] metadata, List<RouteSpec> specs) throws Exception;
}
