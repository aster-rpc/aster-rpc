package site.aster.server;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.Objects;
import java.util.concurrent.CancellationException;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.upcall.Upcalls;

/**
 * An owned, bind-ready HTTPS/optional WebTransport listener (ABI 4.12). The parent server and
 * runtime also own its lifetime. Closing the listener synchronously joins native calls and
 * connections and releases its TCP/UDP ports. Address strings remain available after close.
 */
public final class HttpListener implements AutoCloseable {
  private static final ValueLayout.OfInt I = ValueLayout.JAVA_INT;
  private static final ValueLayout.OfLong J = ValueLayout.JAVA_LONG;
  private static final java.lang.foreign.AddressLayout A = ValueLayout.ADDRESS;
  private static final MemoryLayout BYTES = MemoryLayout.structLayout(A, J);

  /** Native LP64 layout; byte lengths are size_t, not u32. */
  public static final MemoryLayout CONFIG_LAYOUT =
      MemoryLayout.structLayout(I, I, BYTES, BYTES, BYTES, I, I);

  private static final MethodHandle START =
      h("aster_server_http_start", FunctionDescriptor.of(I, J, J, J, A, A));
  private static final MethodHandle START_PUBLIC =
      h("aster_server_http_start_public", FunctionDescriptor.of(I, J, J, A, A));
  private static final MethodHandle TCP_ADDR =
      h("aster_http_listener_tcp_addr", FunctionDescriptor.of(I, J, J, A, A, A));
  private static final MethodHandle QUIC_ADDR =
      h("aster_http_listener_quic_addr", FunctionDescriptor.of(I, J, J, A, A, A));
  private static final MethodHandle STOP =
      h("aster_http_listener_stop", FunctionDescriptor.of(I, J, J, I));
  private static final MethodHandle RELEASE =
      h("aster_buffer_release", FunctionDescriptor.of(I, J, J));

  private static MethodHandle h(String name, FunctionDescriptor descriptor) {
    return IrohLibrary.getInstance().getHandle(name, descriptor);
  }

  // Keep the table/runtime reachable while native listener callbacks may use them. Explicit
  // runtime close remains authoritative and performs forced child cleanup.
  private final Upcalls upcalls;
  private final long runtime;
  private long handle;
  private final String tcpAddress;
  private final String quicAddress;

  private HttpListener(Upcalls upcalls, long runtime, long handle, String tcp, String quic) {
    this.upcalls = upcalls;
    this.runtime = runtime;
    this.handle = handle;
    this.tcpAddress = tcp;
    this.quicAddress = quic;
  }

  /**
   * FFM bridge used by Dispatch. Call {@link AsterServer#startHttp} for the high-level API. The
   * caller must serialize this operation against destruction of the parent server.
   */
  public static HttpListener start(
      long runtime, long server, Upcalls upcalls, HttpListenerConfig config) {
    Objects.requireNonNull(upcalls, "upcalls");
    Objects.requireNonNull(config, "config");
    if (upcalls.runtime().nativeHandle() != runtime) {
      throw new IllegalArgumentException("upcalls must belong to the server runtime");
    }
    if (!upcalls.hasRpcAuthenticator()) {
      throw new IllegalArgumentException(
          "register an RPC authenticator before starting a listener");
    }
    return startInternal(runtime, server, upcalls, config);
  }

  /** Explicit anonymous listener; no upcall table is needed. Session services are disabled. */
  public static HttpListener startPublic(long runtime, long server, HttpListenerConfig config) {
    Objects.requireNonNull(config, "config");
    if (config.maxPendingAuth() != 0 || config.maxAuthPayloadBytes() != 0) {
      throw new IllegalArgumentException("public listeners do not accept authentication limits");
    }
    return startInternal(runtime, server, null, config);
  }

  private static HttpListener startInternal(
      long runtime, long server, Upcalls upcalls, HttpListenerConfig config) {
    if (Thread.currentThread().isInterrupted()) {
      throw new CancellationException("listener startup interrupted");
    }
    long handle = 0;
    HttpListener listener = null;
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment cfg = arena.allocate(CONFIG_LAYOUT);
      cfg.set(I, 0, (int) CONFIG_LAYOUT.byteSize());
      cfg.set(I, 4, config.webTransport() ? 1 : 0);
      putBytes(arena, cfg, 8, config.bindAddress().getBytes(StandardCharsets.UTF_8));
      putBytes(arena, cfg, 24, config.certificatePem());
      putBytes(arena, cfg, 40, config.privateKeyPem());
      cfg.set(I, 56, config.maxPendingAuth());
      cfg.set(I, 60, config.maxAuthPayloadBytes());
      MemorySegment out = arena.allocate(J);
      check(
          upcalls == null
              ? (int) START_PUBLIC.invoke(runtime, server, cfg, out)
              : (int) START.invoke(runtime, server, upcalls.nativeHandle(), cfg, out),
          "listener start");
      handle = out.get(J, 0);
      String tcp = address(TCP_ADDR, runtime, handle);
      String quic = config.webTransport() ? address(QUIC_ADDR, runtime, handle) : null;
      listener = new HttpListener(upcalls, runtime, handle, tcp, quic);
      if (Thread.currentThread().isInterrupted()) {
        throw new CancellationException("listener startup interrupted");
      }
      return listener;
    } catch (Throwable error) {
      if (handle != 0) {
        try {
          if (listener != null) listener.close();
          else checkStop((int) STOP.invoke(runtime, handle, 0));
        } catch (Throwable cleanup) {
          // Preserve an explicit managed owner even when the address copy failed. Native parent
          // ownership is a fallback; callers can retry deterministic shutdown through this owner.
          if (listener == null) listener = new HttpListener(upcalls, runtime, handle, "", null);
          throw new HttpListenerStartException(listener, error, cleanup);
        }
      }
      throw rethrow(error);
    }
  }

  private static void putBytes(Arena arena, MemorySegment cfg, long offset, byte[] bytes) {
    cfg.set(A, offset, arena.allocateFrom(ValueLayout.JAVA_BYTE, bytes));
    cfg.set(J, offset + 8, bytes.length);
  }

  private static String address(MethodHandle operation, long runtime, long handle)
      throws Throwable {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment buffer = arena.allocate(J);
      MemorySegment pointer = arena.allocate(A);
      MemorySegment length = arena.allocate(I);
      check((int) operation.invoke(runtime, handle, buffer, pointer, length), "listener address");
      try {
        long size = Integer.toUnsignedLong(length.get(I, 0));
        return new String(
            pointer.get(A, 0).reinterpret(size).toArray(ValueLayout.JAVA_BYTE),
            StandardCharsets.UTF_8);
      } finally {
        check((int) RELEASE.invoke(runtime, buffer.get(J, 0)), "listener address release");
      }
    }
  }

  public String tcpAddress() {
    return tcpAddress;
  }

  /** Null when WebTransport was disabled. */
  public String quicAddress() {
    return quicAddress;
  }

  public synchronized boolean isClosed() {
    return handle == 0;
  }

  /**
   * Allow active calls to drain for grace, then cancel and join remaining work. Grace must fit an
   * unsigned 32-bit millisecond count. Blocks; run off latency-sensitive/callback threads. A failed
   * stop keeps the owner available for retry. Parent/runtime shutdown makes this a harmless no-op.
   */
  public synchronized void stop(Duration grace) {
    Objects.requireNonNull(grace, "grace");
    long milliseconds = grace.toMillis();
    if (grace.isNegative() || milliseconds > 0xffff_ffffL) {
      throw new IllegalArgumentException("grace must be 0..4294967295 milliseconds");
    }
    if (handle == 0) return;
    try {
      checkStop((int) STOP.invoke(runtime, handle, (int) milliseconds));
      handle = 0;
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  @Override
  public void close() {
    stop(Duration.ZERO);
  }

  private static void checkStop(int status) {
    if (status == IrohStatus.NOT_FOUND.code || status == IrohStatus.ALREADY_CLOSED.code) return;
    check(status, "listener stop");
  }

  private static void check(int status, String operation) {
    if (status != 0) {
      throw IrohException.forStatus(
          IrohStatus.fromCode(status), operation + " failed: " + DataPlane.lastError());
    }
  }

  private static RuntimeException rethrow(Throwable t) {
    if (t instanceof RuntimeException e) return e;
    if (t instanceof Error e) throw e;
    return new RuntimeException(t);
  }
}
