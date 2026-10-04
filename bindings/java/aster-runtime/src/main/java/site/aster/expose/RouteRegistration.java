package site.aster.expose;

import java.lang.foreign.FunctionDescriptor;
import java.lang.invoke.MethodHandle;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.UpcallNative;

/**
 * A live route registration ({@code aster_route_registration_t}): the edge keeps the granted routes
 * while it is held; {@link #close()} (once) evicts them.
 */
public final class RouteRegistration implements AutoCloseable {

  private static final MethodHandle FREE =
      UpcallNative.handle(
          "aster_route_registration_free",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J));

  private final IrohRuntime runtime;
  private final long handle;
  private final List<RouteSpec> requested;
  private final List<Integer> granted;
  private final AtomicBoolean closed = new AtomicBoolean();

  RouteRegistration(
      IrohRuntime runtime, long handle, List<RouteSpec> requested, List<Integer> granted) {
    this.runtime = runtime;
    this.handle = handle;
    this.requested = List.copyOf(requested);
    this.granted = List.copyOf(granted);
  }

  public long nativeHandle() {
    return handle;
  }

  /** Indexes into the requested specs the edge granted, ascending. */
  public List<Integer> grantedIndexes() {
    return granted;
  }

  /** The granted specs, in request order. */
  public List<RouteSpec> granted() {
    return granted.stream().map(requested::get).toList();
  }

  @Override
  public void close() {
    if (closed.compareAndSet(false, true)) {
      try {
        FREE.invoke(runtime.nativeHandle(), handle);
      } catch (Throwable ignored) {
        // runtime already closed
      }
    }
  }
}
