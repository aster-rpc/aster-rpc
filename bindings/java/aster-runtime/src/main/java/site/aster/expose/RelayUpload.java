package site.aster.expose;

import java.io.IOException;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.Arrays;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.UpcallNative;

/**
 * The request body of a streaming relay ({@code aster_http_upload_t}). Write chunks, then {@link
 * #finish}; {@link #close()} releases the handle and <b>aborts</b> an unfinished upload (the
 * handler sees a failed body, never a truncated one). Writes wait without pinning a carrier thread.
 * {@link HttpBodyException.Kind#CANCELLED} means the handler stopped reading (it may already have
 * answered): stop producing.
 */
public final class RelayUpload implements AutoCloseable {

  private static final MethodHandle TRY_WRITE =
      UpcallNative.handle(
          "aster_http_upload_try_write",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              UpcallNative.A,
              UpcallNative.J,
              UpcallNative.A));
  private static final MethodHandle WRITE =
      UpcallNative.handle(
          "aster_http_upload_write",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              UpcallNative.A,
              UpcallNative.J,
              UpcallNative.I));
  private static final MethodHandle FINISH =
      UpcallNative.handle(
          "aster_http_upload_finish",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J));
  private static final MethodHandle ABORT =
      UpcallNative.handle(
          "aster_http_upload_abort",
          FunctionDescriptor.of(
              UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.A, UpcallNative.J));
  private static final MethodHandle READY =
      UpcallNative.handle(
          "aster_http_upload_ready",
          FunctionDescriptor.of(
              UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.J, UpcallNative.A));
  private static final MethodHandle FREE =
      UpcallNative.handle(
          "aster_http_upload_free",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J));

  private final IrohRuntime runtime;
  private final long handle;
  private final long maxChunk;
  private final AtomicBoolean closed = new AtomicBoolean();

  RelayUpload(IrohRuntime runtime, long handle, long maxChunk) {
    this.runtime = runtime;
    this.handle = handle;
    this.maxChunk = maxChunk;
  }

  public long nativeHandle() {
    return handle;
  }

  /** Queue {@code chunk} (copied; split at the relay's {@code maxChunk}), waiting while full. */
  public void write(byte[] chunk) throws IOException {
    write(chunk, 0, chunk.length, null);
  }

  /** {@link #write(byte[])} of a slice, giving up after {@code timeout} ({@code null}: never). */
  public void write(byte[] b, int off, int len, Duration timeout) throws IOException {
    java.util.Objects.checkFromIndexSize(off, len, b.length);
    int at = off;
    int end = off + len;
    long deadline = HttpBodies.deadline(timeout);
    while (at < end) {
      int n = (int) Math.min(maxChunk, end - at);
      writeOne(Arrays.copyOfRange(b, at, at + n), deadline);
      at += n;
    }
  }

  private void writeOne(byte[] chunk, long deadline) throws IOException {
    try (Arena a = Arena.ofConfined()) {
      MemorySegment seg = a.allocate(chunk.length);
      seg.copyFrom(MemorySegment.ofArray(chunk));
      MemorySegment queued = a.allocate(UpcallNative.I);
      long park = 0;
      while (true) {
        int rc =
            (int)
                TRY_WRITE.invoke(runtime.nativeHandle(), handle, seg, (long) chunk.length, queued);
        if (rc != 0) {
          throw HttpBodies.failure(rc, "aster_http_upload_try_write");
        }
        if (queued.get(UpcallNative.I, 0) == 1) {
          return;
        }
        park = HttpBodies.awaitRetry(park, deadline);
      }
    } catch (IOException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /**
   * {@link #write} through the native blocking call: parks the calling thread inside native code
   * (pinning a virtual thread's carrier); platform threads only. A timed-out write queued nothing.
   */
  public void writeBlocking(byte[] chunk, Duration timeout) throws IOException {
    if (chunk.length > maxChunk) {
      throw new HttpBodyException(HttpBodyException.Kind.CHUNK_TOO_LARGE, "chunk over maxChunk");
    }
    long ms = Math.max(0, Math.min(Integer.MAX_VALUE, timeout.toMillis()));
    try (Arena a = Arena.ofConfined()) {
      MemorySegment seg = a.allocate(Math.max(1, chunk.length));
      seg.copyFrom(MemorySegment.ofArray(chunk));
      int rc =
          (int) WRITE.invoke(runtime.nativeHandle(), handle, seg, (long) chunk.length, (int) ms);
      if (rc != 0) {
        throw HttpBodies.failure(rc, "aster_http_upload_write");
      }
    } catch (IOException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /** End the request body cleanly; queued chunks are still sent. Never waits. */
  public void finish() throws IOException {
    int rc;
    try {
      rc = (int) FINISH.invoke(runtime.nativeHandle(), handle);
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
    if (rc != 0) {
      throw HttpBodies.failure(rc, "aster_http_upload_finish");
    }
  }

  /**
   * End the request body with an error: queued chunks are discarded and the request stream is
   * reset. Before the head, the response then fails with {@link
   * HttpBodyException.Kind#UPLOAD_FAILED}. {@code message} stays local. Never waits.
   */
  public void abort(String message) throws IOException {
    byte[] raw = (message == null ? "" : message).getBytes(StandardCharsets.UTF_8);
    try (Arena a = Arena.ofConfined()) {
      MemorySegment seg = MemorySegment.NULL;
      if (raw.length > 0) {
        seg = a.allocate(raw.length);
        seg.copyFrom(MemorySegment.ofArray(raw));
      }
      int rc = (int) ABORT.invoke(runtime.nativeHandle(), handle, seg, (long) raw.length);
      if (rc != 0) {
        throw HttpBodies.failure(rc, "aster_http_upload_abort");
      }
    } catch (IOException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /**
   * One-shot readiness: completes with the {@code aster_http_body_status_t} the next {@link
   * #tryWrite} would return (0: room to write; {@code FINISHED} / {@code CANCELLED}: every write
   * would fail). Readiness only: another writer may take the room first. Re-arm after handling;
   * closing the upload resolves it.
   */
  public CompletableFuture<Integer> ready() {
    return UpcallNative.submit(
            runtime,
            "aster_http_upload_ready",
            (a, out) -> (int) READY.invoke(runtime.nativeHandle(), handle, 0L, out))
        .thenApply(r -> r.event().errorCode());
  }

  /** Queue one chunk (at most {@code maxChunk}) if there is room now; false = full. */
  public boolean tryWrite(byte[] chunk) throws IOException {
    try (Arena a = Arena.ofConfined()) {
      MemorySegment seg = MemorySegment.NULL;
      if (chunk.length > 0) {
        seg = a.allocate(chunk.length);
        seg.copyFrom(MemorySegment.ofArray(chunk));
      }
      MemorySegment queued = a.allocate(UpcallNative.I);
      int rc =
          (int) TRY_WRITE.invoke(runtime.nativeHandle(), handle, seg, (long) chunk.length, queued);
      if (rc != 0) {
        throw HttpBodies.failure(rc, "aster_http_upload_try_write");
      }
      return queued.get(UpcallNative.I, 0) == 1;
    } catch (IOException | RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /** Release the handle (once); an unfinished upload is aborted. */
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
