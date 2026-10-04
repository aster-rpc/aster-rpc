package site.aster.expose;

import java.io.IOException;
import java.io.InputStream;
import java.io.InterruptedIOException;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.locks.LockSupport;
import site.aster.ffi.DataPlane;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.Ucv1;
import site.aster.upcall.Ucv1.HttpHeader;
import site.aster.upcall.UpcallNative;

/**
 * A body-handle table for foreign HTTP exchanges ({@code aster_http_bodies_t}, C ABI 4.2): the
 * request body of an {@code expose_http} upcall is read, and a streamed response body written, by
 * the request's {@code stream_id}.
 *
 * <p>Every call here is <b>non-blocking in native code</b>: the C functions would park the calling
 * thread (and, on a virtual thread, pin its carrier), so reads and writes poll with a zero timeout
 * and park between attempts with a short back-off (at most {@link #MAX_IDLE_PARK}), the same
 * discipline as the upcall poll loop. A waiting call ends with {@link InterruptedIOException} when
 * its thread is interrupted (an upcall cancelled by Rust, the table closing).
 *
 * <p>Unless you serve the table yourself, {@link Expose} creates and owns one.
 */
public final class HttpBodies implements AutoCloseable {

  /** Longest park between two attempts of a waiting call. */
  public static final Duration MAX_IDLE_PARK = Duration.ofMillis(2);

  private static final int STATUS_TIMEOUT = 6;
  private static final int MAX_READ = 1 << 20;

  private static final ValueLayout.OfInt I = UpcallNative.I;
  private static final ValueLayout.OfLong J = UpcallNative.J;
  private static final java.lang.foreign.AddressLayout A = UpcallNative.A;

  private static final MethodHandle NEW =
      UpcallNative.handle("aster_http_bodies_new", FunctionDescriptor.of(I, J, A));
  private static final MethodHandle FREE =
      UpcallNative.handle("aster_http_bodies_free", FunctionDescriptor.of(I, J, J));
  private static final MethodHandle OPEN_COUNT =
      UpcallNative.handle("aster_http_bodies_open_count", FunctionDescriptor.of(I, J, J, A));
  private static final MethodHandle READ =
      UpcallNative.handle("aster_http_body_read", FunctionDescriptor.of(I, J, J, J, A, J, I, A, A));
  private static final MethodHandle WRITE =
      UpcallNative.handle("aster_http_body_write", FunctionDescriptor.of(I, J, J, J, A, J, I));
  private static final MethodHandle TRY_WRITE =
      UpcallNative.handle("aster_http_body_try_write", FunctionDescriptor.of(I, J, J, J, A, J, A));
  private static final MethodHandle FINISH =
      UpcallNative.handle("aster_http_body_finish", FunctionDescriptor.of(I, J, J, J, A, J));
  private static final MethodHandle ABORT =
      UpcallNative.handle("aster_http_body_abort", FunctionDescriptor.of(I, J, J, J, A, J));

  private final IrohRuntime runtime;
  private final long handle;
  private final AtomicBoolean closed = new AtomicBoolean();

  private HttpBodies(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
  }

  /** A new table for {@code runtime}. */
  public static HttpBodies create(IrohRuntime runtime) {
    long h =
        UpcallNative.callForLong(
            "aster_http_bodies_new", (a, out) -> (int) NEW.invoke(runtime.nativeHandle(), out));
    return new HttpBodies(runtime, h);
  }

  public IrohRuntime runtime() {
    return runtime;
  }

  public long nativeHandle() {
    return handle;
  }

  /** Open exchanges (leak checks and tests). */
  public long openCount() {
    return UpcallNative.callForLong(
        "aster_http_bodies_open_count",
        (a, out) -> (int) OPEN_COUNT.invoke(runtime.nativeHandle(), handle, out));
  }

  // -- reads ---------------------------------------------------------------------------------

  /**
   * Read up to {@code max} bytes of the request body of exchange {@code streamId}; at most the
   * service's {@code maxChunk} come back. Returns {@code null} at the end of the body; a short read
   * is normal. Pull-based: Rust reads the QUIC stream only while this is called. {@code timeout}
   * {@code null} waits as long as it takes; a timed-out read consumes nothing and throws {@link
   * HttpBodyException.Kind#TIMEOUT}.
   */
  public byte[] read(long streamId, int max, Duration timeout) throws IOException {
    if (max <= 0) {
      throw new IllegalArgumentException("max must be > 0");
    }
    int cap = Math.min(max, MAX_READ);
    long deadline = deadline(timeout);
    try (Arena a = Arena.ofConfined()) {
      MemorySegment buf = a.allocate(cap);
      MemorySegment outLen = a.allocate(J);
      MemorySegment outEof = a.allocate(I);
      long park = 0;
      while (true) {
        int rc =
            (int)
                READ.invoke(
                    runtime.nativeHandle(), handle, streamId, buf, (long) cap, 0, outLen, outEof);
        if (rc == 0) {
          if (outEof.get(I, 0) == 1) {
            return null;
          }
          int n = (int) outLen.get(J, 0);
          if (n > 0) {
            return buf.asSlice(0, n).toArray(ValueLayout.JAVA_BYTE);
          }
        } else if (rc != STATUS_TIMEOUT) {
          throw failure(rc, "aster_http_body_read");
        }
        park = awaitRetry(park, deadline);
      }
    } catch (IOException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /** The request body as a stream ({@code read} waits for data; end of body is end of stream). */
  public InputStream requestBody(long streamId) {
    return new RequestBodyStream(streamId);
  }

  private final class RequestBodyStream extends InputStream {
    private final long streamId;
    private byte[] pending = new byte[0];
    private int at;
    private boolean eof;

    RequestBodyStream(long streamId) {
      this.streamId = streamId;
    }

    private boolean fill(int want) throws IOException {
      while (at >= pending.length) {
        if (eof) {
          return false;
        }
        byte[] next = HttpBodies.this.read(streamId, Math.max(want, 8192), null);
        if (next == null) {
          eof = true;
          return false;
        }
        pending = next;
        at = 0;
      }
      return true;
    }

    @Override
    public int read() throws IOException {
      if (!fill(1)) {
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
      if (!fill(len)) {
        return -1;
      }
      int n = Math.min(len, pending.length - at);
      System.arraycopy(pending, at, b, off, n);
      at += n;
      return n;
    }

    @Override
    public int available() {
      return pending.length - at;
    }
  }

  // -- writes --------------------------------------------------------------------------------

  /**
   * Queue one response-body chunk (copied; at most the service's {@code maxChunk}), waiting while
   * the response buffer is full. {@link HttpBodyException.Kind#CANCELLED} means the consumer is
   * gone: stop producing. Respond with the head (body mode {@code Stream}) first, or write at most
   * {@code responseBuffer} chunks before it.
   */
  public void write(long streamId, byte[] chunk, Duration timeout) throws IOException {
    if (chunk.length == 0) {
      return;
    }
    long deadline = deadline(timeout);
    try (Arena a = Arena.ofConfined()) {
      MemorySegment seg = a.allocate(chunk.length);
      seg.copyFrom(MemorySegment.ofArray(chunk));
      MemorySegment queued = a.allocate(I);
      long park = 0;
      while (true) {
        int rc =
            (int)
                TRY_WRITE.invoke(
                    runtime.nativeHandle(), handle, streamId, seg, (long) chunk.length, queued);
        if (rc != 0) {
          throw failure(rc, "aster_http_body_try_write");
        }
        if (queued.get(I, 0) == 1) {
          return;
        }
        park = awaitRetry(park, deadline);
      }
    } catch (IOException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /**
   * {@link #write} through the native blocking call ({@code aster_http_body_write}, waiting up to
   * {@code timeout}): it parks the calling thread inside native code, so on a virtual thread it
   * pins the carrier. Prefer {@link #write}; use this from platform threads only. A timed-out write
   * queued nothing ({@link HttpBodyException.Kind#TIMEOUT}).
   */
  public void writeBlocking(long streamId, byte[] chunk, Duration timeout) throws IOException {
    if (chunk.length == 0) {
      return;
    }
    long ms = Math.max(0, Math.min(Integer.MAX_VALUE, timeout.toMillis()));
    try (Arena a = Arena.ofConfined()) {
      MemorySegment seg = a.allocate(chunk.length);
      seg.copyFrom(MemorySegment.ofArray(chunk));
      int rc =
          (int)
              WRITE.invoke(
                  runtime.nativeHandle(), handle, streamId, seg, (long) chunk.length, (int) ms);
      if (rc != 0) {
        throw failure(rc, "aster_http_body_write");
      }
    } catch (IOException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /** Queue one chunk if there is room now; {@code false} = full, nothing queued. */
  public boolean tryWrite(long streamId, byte[] chunk) throws IOException {
    try (Arena a = Arena.ofConfined()) {
      MemorySegment seg = MemorySegment.NULL;
      if (chunk.length > 0) {
        seg = a.allocate(chunk.length);
        seg.copyFrom(MemorySegment.ofArray(chunk));
      }
      MemorySegment queued = a.allocate(I);
      int rc =
          (int)
              TRY_WRITE.invoke(
                  runtime.nativeHandle(), handle, streamId, seg, (long) chunk.length, queued);
      if (rc != 0) {
        throw failure(rc, "aster_http_body_try_write");
      }
      return queued.get(I, 0) == 1;
    } catch (IOException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /**
   * End the response body cleanly (queued chunks are still delivered), optionally with trailers.
   * Malformed trailers throw {@link HttpBodyException.Kind#INVALID_TRAILERS} and leave the body
   * open. Never waits.
   */
  public void finish(long streamId, List<HttpHeader> trailers) throws IOException {
    byte[] raw =
        trailers == null || trailers.isEmpty()
            ? new byte[0]
            : Ucv1.encodeExposeHttpTrailers(trailers);
    try (Arena a = Arena.ofConfined()) {
      MemorySegment seg = MemorySegment.NULL;
      if (raw.length > 0) {
        seg = a.allocate(raw.length);
        seg.copyFrom(MemorySegment.ofArray(raw));
      }
      int rc =
          (int) FINISH.invoke(runtime.nativeHandle(), handle, streamId, seg, (long) raw.length);
      if (rc != 0) {
        throw failure(rc, "aster_http_body_finish");
      }
    } catch (IOException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /**
   * End the response body with an error: queued chunks are discarded and the relay resets the
   * stream, so the consumer sees a failed response, never a truncated one. {@code message} is
   * logged, not sent. Never waits.
   */
  public void abort(long streamId, String message) throws IOException {
    byte[] raw = (message == null ? "" : message).getBytes(java.nio.charset.StandardCharsets.UTF_8);
    try (Arena a = Arena.ofConfined()) {
      MemorySegment seg = MemorySegment.NULL;
      if (raw.length > 0) {
        seg = a.allocate(raw.length);
        seg.copyFrom(MemorySegment.ofArray(raw));
      }
      int rc = (int) ABORT.invoke(runtime.nativeHandle(), handle, streamId, seg, (long) raw.length);
      if (rc != 0) {
        throw failure(rc, "aster_http_body_abort");
      }
    } catch (IOException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  // -- lifecycle -----------------------------------------------------------------------------

  /**
   * Free the table (once). Services registered with it keep their own reference and keep working;
   * the binding just cannot reach their bodies any more, so close it after the services stop.
   */
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

  // -- helpers -------------------------------------------------------------------------------

  static long deadline(Duration timeout) {
    if (timeout == null) {
      return Long.MAX_VALUE;
    }
    long now = System.nanoTime();
    long add = timeout.toNanos();
    return add > Long.MAX_VALUE - now ? Long.MAX_VALUE : now + add;
  }

  /** Park with back-off; throws when interrupted or past {@code deadline}. */
  static long awaitRetry(long park, long deadline) throws IOException {
    if (Thread.currentThread().isInterrupted()) {
      throw new InterruptedIOException("body call interrupted");
    }
    if (deadline != Long.MAX_VALUE && System.nanoTime() - deadline >= 0) {
      throw new HttpBodyException(HttpBodyException.Kind.TIMEOUT, "body call timed out");
    }
    long next = park == 0 ? 50_000 : Math.min(park * 2, MAX_IDLE_PARK.toNanos());
    LockSupport.parkNanos(next);
    return next;
  }

  static HttpBodyException failure(int rc, String fn) {
    return new HttpBodyException(rc, fn + " failed (" + rc + "): " + DataPlane.lastError());
  }
}
