package site.aster.health;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.node.IrohNode;
import site.aster.otel.Observability;

/**
 * The ops listener (design §6): liveness, readiness and Prometheus scrape endpoints served by Rust
 * ({@code aster::ops}, {@code aster_health_*}).
 *
 * <ul>
 *   <li>{@code GET /healthz}: 200 {@code {"status":"ok","uptime_s":...}}, or 503 once {@link
 *       #setHealthy} is false;
 *   <li>{@code GET /readyz}: 200 when the server is ready, healthy and every named {@link #check}
 *       passes, else 503 with {@code {"status","services","registry","checks":{name:bool}}};
 *   <li>{@code GET /metrics}: Prometheus text of the built-in {@code aster.*} instruments and your
 *       {@link site.aster.otel.Metrics} (503 under {@code OTEL_SDK_DISABLED=true}).
 * </ul>
 *
 * <p>The listener joins the observability pipeline (starting an environment-only one if none is
 * running) and gives it a Prometheus pull reader whatever {@code OTEL_METRICS_EXPORTER} says. It
 * binds {@code 127.0.0.1} by default; probes in pods need an explicit {@code 0.0.0.0}. Attach it to
 * a server with {@code AsterServer.Builder#health}: a successful start sets ready, closing the
 * server clears it first.
 *
 * <p>{@code start} blocks until the socket is bound (the C ABI refuses it on a tokio thread; a Java
 * thread is fine).
 */
public final class HealthServer implements AutoCloseable {

  private static final java.lang.foreign.AddressLayout PTR = ValueLayout.ADDRESS;
  private static final ValueLayout.OfInt I32 = ValueLayout.JAVA_INT;
  private static final ValueLayout.OfLong I64 = ValueLayout.JAVA_LONG;
  private static final ValueLayout.OfBoolean BOOL = ValueLayout.JAVA_BOOLEAN;

  /** {@code sizeof(aster_health_config_t)}. */
  private static final long CONFIG_SIZE = 24;

  private static MethodHandle h(String name, FunctionDescriptor d) {
    return IrohLibrary.getInstance().getHandle(name, d);
  }

  private static final MethodHandle START =
      h("aster_health_start", FunctionDescriptor.of(I32, I64, I64, PTR, PTR));
  private static final MethodHandle SET_READY =
      h("aster_health_set_ready", FunctionDescriptor.of(I32, I64, BOOL));
  private static final MethodHandle SET_HEALTHY =
      h("aster_health_set_healthy", FunctionDescriptor.of(I32, I64, BOOL));
  private static final MethodHandle CHECK_SET =
      h(
          "aster_health_check_set",
          FunctionDescriptor.of(I32, I64, site.aster.ffi.IrohLibrary.ASTER_BYTES, BOOL));
  private static final MethodHandle ADDR =
      h("aster_health_addr", FunctionDescriptor.of(I32, I64, PTR));
  private static final MethodHandle STOP =
      h("aster_health_stop", FunctionDescriptor.of(I32, I64, I64));

  private final long runtimeHandle;
  private final long handle;
  private final String address;
  private final AtomicBoolean closed = new AtomicBoolean();

  private HealthServer(long runtimeHandle, long handle, String address) {
    this.runtimeHandle = runtimeHandle;
    this.handle = handle;
    this.address = address;
  }

  /**
   * Start the listener with the node's {@code AsterConfig} health settings ({@code
   * ASTER_HEALTH_PORT} / {@code _HOST} / {@code _ENABLED}, {@code [health]}).
   *
   * @throws IllegalArgumentException when that configuration has health disabled
   */
  public static HealthServer start(IrohNode node) {
    return start(node, null, 0, false);
  }

  /**
   * Start the listener on {@code host}:{@code port} ({@code port} 0 picks an ephemeral port; read
   * it with {@link #address()}).
   */
  public static HealthServer start(IrohNode node, String host, int port) {
    if (port < 0 || port > 65535) {
      throw new IllegalArgumentException("port out of range: " + port);
    }
    return start(node, host, port, true);
  }

  private static HealthServer start(IrohNode node, String host, int port, boolean explicit) {
    Observability.ensureStarted();
    long rt = node.runtime().nativeHandle();
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment cfg = MemorySegment.NULL;
      if (explicit) {
        cfg = arena.allocate(CONFIG_SIZE, 8);
        cfg.set(I32, 0, (int) CONFIG_SIZE);
        cfg.set(ValueLayout.JAVA_SHORT, 4, (short) port);
        byte[] hb = (host == null ? "" : host).getBytes(StandardCharsets.UTF_8);
        if (hb.length > 0) {
          cfg.set(PTR, 8, arena.allocateFrom(ValueLayout.JAVA_BYTE, hb));
          cfg.set(I64, 16, hb.length);
        }
      }
      MemorySegment out = arena.allocate(8, 8);
      int status = (int) START.invoke(rt, node.nodeHandle(), cfg, out);
      if (status == IrohStatus.INVALID_ARGUMENT.code) {
        throw new IllegalArgumentException("aster_health_start: " + DataPlane.lastError());
      }
      checkStatus(status, "aster_health_start");
      long h = out.get(I64, 0);
      return new HealthServer(rt, h, readAddress(h));
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  private static String readAddress(long h) throws Throwable {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(16, 8);
      checkStatus((int) ADDR.invoke(h, out), "aster_health_addr");
      MemorySegment ptr = out.get(PTR, 0);
      long len = out.get(I64, 8);
      if (ptr.equals(MemorySegment.NULL) || len == 0) {
        return "";
      }
      String text =
          new String(ptr.reinterpret(len).toArray(ValueLayout.JAVA_BYTE), StandardCharsets.UTF_8);
      IrohLibrary.getInstance().stringRelease(ptr, len);
      return text;
    }
  }

  /** The bound address as {@code "ip:port"} (the real port after port 0). */
  public String address() {
    return address;
  }

  /** The bound port. */
  public int port() {
    return Integer.parseInt(address.substring(address.lastIndexOf(':') + 1));
  }

  /** The native {@code aster_health_t}, for {@code Dispatch.Builder#attachHealth}. */
  public long nativeHandle() {
    return handle;
  }

  /**
   * Set the ready flag ({@code /readyz}). A server this listener is attached to sets it on a
   * successful start and clears it when it closes; this is for your own gating.
   */
  public void setReady(boolean ready) {
    try {
      checkStatus((int) SET_READY.invoke(handle, ready), "aster_health_set_ready");
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /** Set liveness ({@code /healthz}); unhealthy also fails {@code /readyz}. */
  public void setHealthy(boolean healthy) {
    try {
      checkStatus((int) SET_HEALTHY.invoke(handle, healthy), "aster_health_set_healthy");
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * The named readiness check, <em>created failing</em>: a declared dependency is not ready until
   * it says so. Every check must pass for {@code /readyz}, which lists them under {@code checks}.
   */
  public Check check(String name) {
    return new Check(name);
  }

  /** One named readiness check. */
  public final class Check {
    private final String name;

    private Check(String name) {
      this.name = name;
    }

    /** Set the check's state ({@code /readyz} reports it under {@code checks}). */
    public void set(boolean ok) {
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment n = arena.allocate(16, 8);
        byte[] nb = name.getBytes(StandardCharsets.UTF_8);
        n.set(PTR, 0, arena.allocateFrom(ValueLayout.JAVA_BYTE, nb));
        n.set(I64, 8, nb.length);
        checkStatus((int) CHECK_SET.invoke(handle, n, ok), "aster_health_check_set");
      } catch (Throwable t) {
        throw rethrow(t);
      }
    }
  }

  /** Stop the listener and free the handle; the port is free on return. Idempotent. */
  @Override
  public void close() {
    if (!closed.compareAndSet(false, true)) {
      return;
    }
    try {
      int status = (int) STOP.invoke(runtimeHandle, handle);
      if (status != IrohStatus.NOT_FOUND.code) {
        checkStatus(status, "aster_health_stop");
      }
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  private static void checkStatus(int status, String op) {
    if (status != IrohStatus.OK.code) {
      IrohStatus s = IrohStatus.fromCode(status);
      String detail = DataPlane.lastError();
      throw IrohException.forStatus(
          s, op + " failed: " + s.name() + (detail.isEmpty() ? "" : " (" + detail + ")"));
    }
  }

  private static RuntimeException rethrow(Throwable t) {
    if (t instanceof RuntimeException re) {
      return re;
    }
    if (t instanceof Error err) {
      throw err;
    }
    return new IllegalStateException(t);
  }
}
