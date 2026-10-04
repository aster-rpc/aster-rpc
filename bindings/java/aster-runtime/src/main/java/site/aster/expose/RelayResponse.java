package site.aster.expose;

import java.io.IOException;
import java.io.InputStream;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.List;
import java.util.Locale;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.Ucv1;
import site.aster.upcall.Ucv1.HttpHeader;
import site.aster.upcall.UpcallNative;

/**
 * The response of a streaming relay ({@code aster_http_response_t}): the head ({@link #status},
 * {@link #headers}) and a body to read. Reads wait without pinning a carrier thread; a response
 * that fails mid-body throws {@link HttpBodyException} ({@link HttpBodyException.Kind#READ}) from
 * the read, never a clean end.
 *
 * <p><b>Close it.</b> Closing before the body was read to its end abandons the exchange: the
 * handler's writes fail and an unfinished upload is reset. A body read to its clean end leaves the
 * upload to finish.
 */
public final class RelayResponse implements AutoCloseable {

  private static final MethodHandle WAIT =
      UpcallNative.handle(
          "aster_http_response_wait",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              UpcallNative.I,
              UpcallNative.A,
              UpcallNative.J,
              UpcallNative.A));
  private static final MethodHandle READ =
      UpcallNative.handle(
          "aster_http_response_read",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              UpcallNative.A,
              UpcallNative.J,
              UpcallNative.I,
              UpcallNative.A,
              UpcallNative.A));
  private static final MethodHandle READY =
      UpcallNative.handle(
          "aster_http_response_ready",
          FunctionDescriptor.of(
              UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.J, UpcallNative.A));
  private static final MethodHandle CLOSE =
      UpcallNative.handle(
          "aster_http_response_close",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J));
  private static final MethodHandle FREE =
      UpcallNative.handle(
          "aster_http_response_free",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J));
  private static final MethodHandle TO_STATUS =
      UpcallNative.handle(
          "aster_http_body_status_to_status",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.I));

  private final IrohRuntime runtime;
  private final long handle;
  private final long maxChunk;
  private final Ucv1.ExposeHttpResponse head;
  private final AtomicBoolean closed = new AtomicBoolean();
  private volatile boolean ended;
  private volatile Runnable onAbandon = () -> {};

  private RelayResponse(
      IrohRuntime runtime, long handle, long maxChunk, Ucv1.ExposeHttpResponse head) {
    this.runtime = runtime;
    this.handle = handle;
    this.maxChunk = maxChunk;
    this.head = head;
  }

  /** What the relay does when closed before the body ended (stops a feeding thread). */
  void onAbandon(Runnable r) {
    this.onAbandon = r;
  }

  /**
   * Wait (polling, parking between attempts) for the head of {@code handle}; on failure the handle
   * is freed and an {@link IrohException} of the relay's {@code aster_status_t} is thrown, with
   * {@code cancelled} polled to stop early.
   */
  static RelayResponse awaitHead(
      IrohRuntime runtime, long handle, long maxChunk, java.util.function.BooleanSupplier cancelled)
      throws InterruptedException {
    try (Arena a = Arena.ofConfined()) {
      int cap = 4096;
      MemorySegment buf = a.allocate(cap);
      MemorySegment outLen = a.allocate(UpcallNative.J);
      long park = 0;
      while (true) {
        if (cancelled.getAsBoolean() || Thread.currentThread().isInterrupted()) {
          free(runtime, handle);
          throw new InterruptedException("relay cancelled before the head");
        }
        int rc = (int) WAIT.invoke(runtime.nativeHandle(), handle, 0, buf, (long) cap, outLen);
        if (rc == 0) {
          int n = (int) outLen.get(UpcallNative.J, 0);
          byte[] c5 = buf.asSlice(0, n).toArray(ValueLayout.JAVA_BYTE);
          return new RelayResponse(runtime, handle, maxChunk, Ucv1.decodeExposeHttpResponse(c5));
        }
        if (rc == HttpBodyException.Kind.BUFFER_TOO_SMALL.code) {
          cap = (int) outLen.get(UpcallNative.J, 0);
          buf = a.allocate(cap);
          continue;
        }
        if (rc == HttpBodyException.Kind.TIMEOUT.code) {
          park = Math.min(park == 0 ? 50_000 : park * 2, HttpBodies.MAX_IDLE_PARK.toNanos());
          java.util.concurrent.locks.LockSupport.parkNanos(park);
          continue;
        }
        String detail = site.aster.ffi.DataPlane.lastError();
        free(runtime, handle);
        throw failureFor(rc, detail);
      }
    } catch (InterruptedException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      free(runtime, handle);
      throw UpcallNative.rethrow(t);
    }
  }

  private static IrohException failureFor(int code, String detail) {
    HttpBodyException.Kind kind = HttpBodyException.Kind.fromCode(code);
    int status;
    try {
      status = (int) TO_STATUS.invoke(code);
    } catch (Throwable t) {
      status = IrohStatus.INTERNAL.code;
    }
    return IrohException.forStatus(
        IrohStatus.fromCode(status), "relay failed (" + kind + "): " + detail);
  }

  private static void free(IrohRuntime runtime, long handle) {
    try {
      FREE.invoke(runtime.nativeHandle(), handle);
    } catch (Throwable ignored) {
      // runtime already closed
    }
  }

  public long nativeHandle() {
    return handle;
  }

  public int status() {
    return head.status();
  }

  public List<HttpHeader> headers() {
    return head.headers();
  }

  /** The first value of header {@code name} (case-insensitive), or {@code null}. */
  public byte[] header(String name) {
    String n = name.toLowerCase(Locale.ROOT);
    for (HttpHeader h : head.headers()) {
      if (h.name().equals(n)) {
        return h.value();
      }
    }
    return null;
  }

  /** The body; reading it to the end ({@code -1}) lets the upload finish on its own. */
  public InputStream body() {
    return new Body();
  }

  /** The whole body, at most {@code limit} bytes ({@code < 0}: unbounded). */
  public byte[] readBody(long limit) throws IOException {
    java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
    try (InputStream in = body()) {
      byte[] buf = new byte[8192];
      long total = 0;
      int n;
      while ((n = in.read(buf)) >= 0) {
        total += n;
        if (limit >= 0 && total > limit) {
          throw new HttpBodyException(
              HttpBodyException.Kind.TOO_LARGE, "response body exceeds " + limit + " bytes");
        }
        out.write(buf, 0, n);
      }
    }
    return out.toByteArray();
  }

  /**
   * One-shot readiness of the body: completes with the {@code aster_http_body_status_t} the next
   * read with no wait would return (0: data or the end). Re-arm after handling.
   */
  public CompletableFuture<Integer> ready() {
    return UpcallNative.submit(
            runtime,
            "aster_http_response_ready",
            (a, out) -> (int) READY.invoke(runtime.nativeHandle(), handle, 0L, out))
        .thenApply(r -> r.event().errorCode());
  }

  /** True once the body was read to its clean end. */
  public boolean ended() {
    return ended;
  }

  /** Abandon the exchange but keep the handle (idempotent); reads then fail with CANCELLED. */
  public void cancel() {
    if (!ended) {
      onAbandon.run();
    }
    try {
      CLOSE.invoke(runtime.nativeHandle(), handle);
    } catch (Throwable ignored) {
      // runtime already closed
    }
  }

  /** Close and release the handle (once). */
  @Override
  public void close() {
    if (closed.compareAndSet(false, true)) {
      if (!ended) {
        onAbandon.run();
      }
      free(runtime, handle);
    }
  }

  private final class Body extends InputStream {
    private byte[] pending = new byte[0];
    private int at;
    private boolean eof;

    private boolean fill() throws IOException {
      while (at >= pending.length) {
        if (eof) {
          return false;
        }
        byte[] next = readChunk();
        if (next == null) {
          eof = true;
          ended = true;
          return false;
        }
        pending = next;
        at = 0;
      }
      return true;
    }

    private byte[] readChunk() throws IOException {
      int cap = (int) Math.min(maxChunk, 1 << 20);
      try (Arena a = Arena.ofConfined()) {
        MemorySegment buf = a.allocate(cap);
        MemorySegment outLen = a.allocate(UpcallNative.J);
        MemorySegment outEof = a.allocate(UpcallNative.I);
        long park = 0;
        while (true) {
          int rc =
              (int) READ.invoke(runtime.nativeHandle(), handle, buf, (long) cap, 0, outLen, outEof);
          if (rc == 0) {
            if (outEof.get(UpcallNative.I, 0) == 1) {
              return null;
            }
            int n = (int) outLen.get(UpcallNative.J, 0);
            if (n > 0) {
              return buf.asSlice(0, n).toArray(ValueLayout.JAVA_BYTE);
            }
          } else if (rc != HttpBodyException.Kind.TIMEOUT.code) {
            throw HttpBodies.failure(rc, "aster_http_response_read");
          }
          park = HttpBodies.awaitRetry(park, Long.MAX_VALUE);
        }
      } catch (IOException | RuntimeException e) {
        throw e;
      } catch (Throwable t) {
        throw UpcallNative.rethrow(t);
      }
    }

    @Override
    public int read() throws IOException {
      if (!fill()) {
        return -1;
      }
      return pending[at++] & 0xff;
    }

    @Override
    public int read(byte[] b, int off, int len) throws IOException {
      java.util.Objects.checkFromIndexSize(off, len, b.length);
      if (len == 0) {
        return 0;
      }
      if (!fill()) {
        return -1;
      }
      int n = Math.min(len, pending.length - at);
      System.arraycopy(pending, at, b, off, n);
      at += n;
      return n;
    }

    @Override
    public void close() {
      RelayResponse.this.close();
    }
  }
}
