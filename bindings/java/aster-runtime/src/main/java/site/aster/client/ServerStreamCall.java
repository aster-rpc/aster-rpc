package site.aster.client;

import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.ffi.AsterCall;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.server.AsterFraming;
import site.aster.server.wire.RpcStatus;

/**
 * Incremental server-streaming call. The client has already sent its single request (with {@link
 * AsterFraming#FLAG_END_STREAM}) and now reads response frames one at a time via {@link #recv()}
 * until the server writes a trailer.
 *
 * <p>Contrast with {@link AsterClient#callServerStream} which buffers the full response list into a
 * {@link java.util.concurrent.CompletableFuture} and is only usable for FINITE streams that
 * terminate with a trailer. Use this class when the server produces an open-ended stream (e.g. a
 * log tail) and the caller wants to break out after a few entries without waiting for a trailer
 * that may never arrive.
 *
 * <p>Lifecycle:
 *
 * <pre>{@code
 * try (ServerStreamCall<LogEntry> call =
 *     client.openServerStream(addr, "MissionControl", "tailLogs",
 *                             new TailRequest("", "info"), LogEntry.class).get()) {
 *   LogEntry entry = call.recv();           // block for first entry
 *   // ...decide to stop early...
 * }   // close() discards the underlying stream; reader thread unwinds
 * }</pre>
 */
public final class ServerStreamCall<Resp> implements AutoCloseable {

  private static final Object END_SENTINEL = new Object();

  private final AsterCall call;
  private final Codec codec;
  private final ForyCodec headerCodec;
  private final Class<Resp> responseType;
  private final BlockingQueue<Object> responses = new LinkedBlockingQueue<>();
  private final Thread readerThread;
  private volatile RpcError trailerError;
  private volatile Throwable readerFault;
  private volatile boolean cleanTrailer = false;

  /**
   * Package-private constructor. The caller MUST have already written the StreamHeader and the
   * single request frame (with {@link AsterFraming#FLAG_END_STREAM}) on {@code call}.
   */
  ServerStreamCall(AsterCall call, Codec codec, ForyCodec headerCodec, Class<Resp> responseType) {
    this.call = call;
    this.codec = codec;
    this.headerCodec = headerCodec;
    this.responseType = responseType;
    this.readerThread = Thread.ofVirtual().name("aster-server-stream-reader").start(this::readLoop);
  }

  /**
   * Block until the next response frame arrives or the stream ends. Returns {@code null} on a clean
   * OK trailer. Throws {@link RpcError} on a non-OK trailer or transport error.
   */
  public Resp recv() throws InterruptedException {
    return unwrap(responses.take());
  }

  /**
   * Poll for a response with a timeout. Returns {@code null} on expiry OR on clean end-of-stream;
   * callers distinguish the two via {@link #isComplete()} after a null return.
   */
  public Resp recvWithTimeout(long timeout, TimeUnit unit) throws InterruptedException {
    Object item = responses.poll(timeout, unit);
    if (item == null) return null;
    return unwrap(item);
  }

  /**
   * Send a {@link AsterFraming#FLAG_CANCEL} frame. Spec §5.6 leaves handling up to the server — in
   * particular, the Python server ignores CANCEL on non-session streams. {@link #close} drops the
   * stream regardless, which is the portable way to stop an open-ended server stream.
   */
  public void cancel() {
    try {
      call.sendFrame(AsterFraming.encodeFrame(new byte[0], AsterFraming.FLAG_CANCEL));
    } catch (Throwable ignored) {
      // best-effort; close() will poison the stream either way
    }
  }

  /** True once the reader has observed a clean OK trailer. */
  public boolean isComplete() {
    return cleanTrailer;
  }

  @SuppressWarnings("unchecked")
  private Resp unwrap(Object item) {
    if (item == END_SENTINEL) {
      if (readerFault != null) {
        throw new RpcError(
            StatusCode.INTERNAL,
            readerFault.getMessage() == null ? "reader error" : readerFault.getMessage());
      }
      if (trailerError != null) {
        throw trailerError;
      }
      return null;
    }
    return (Resp) item;
  }

  private void readLoop() {
    try {
      while (true) {
        AsterCall.RecvFrame frame = call.recvFrame(0);
        if (frame instanceof AsterCall.RecvFrame.EndOfStream) {
          readerFault = new IllegalStateException("stream ended before trailer");
          responses.offer(END_SENTINEL);
          return;
        }
        if (frame instanceof AsterCall.RecvFrame.Timeout) {
          // timeout_ms=0 blocks indefinitely — unreachable in practice.
          continue;
        }
        AsterCall.RecvFrame.Ok ok = (AsterCall.RecvFrame.Ok) frame;
        byte flags = ok.flags();
        if ((flags & AsterFraming.FLAG_TRAILER) != 0) {
          RpcStatus status =
              ok.payload().length == 0
                  ? RpcStatus.ok()
                  : (RpcStatus) headerCodec.decode(ok.payload(), RpcStatus.class);
          if (status.code() != RpcStatus.OK) {
            trailerError =
                new RpcError(
                    StatusCode.fromValue(status.code()),
                    status.message() == null ? "" : status.message());
          } else {
            cleanTrailer = true;
          }
          responses.offer(END_SENTINEL);
          return;
        }
        if ((flags & AsterFraming.FLAG_ROW_SCHEMA) != 0) {
          continue;
        }
        Object decoded = codec.decode(ok.payload(), responseType);
        responses.offer(decoded);
      }
    } catch (Throwable t) {
      // Normal when close() discards the stream underneath us — swallow so the shutdown path
      // stays clean.
      readerFault = t;
      responses.offer(END_SENTINEL);
    }
  }

  /**
   * Close the call. Returns the underlying stream to the pool on a clean trailer; discards it on
   * any error/early-exit path so the pool slot is freed. Discarding also tears down the QUIC
   * stream, which is what actually stops an open-ended server generator (CANCEL alone isn't enough
   * — see spec §5.6 / Python server).
   */
  @Override
  public void close() {
    if (!cleanTrailer) {
      // Early exit path: discard first so the reader's next recvFrame unblocks with an error,
      // then join the thread so it doesn't outlive us.
      call.discard();
    }
    if (readerThread.isAlive()) {
      try {
        readerThread.join(2_000);
      } catch (InterruptedException e) {
        Thread.currentThread().interrupt();
      }
    }
    if (cleanTrailer && trailerError == null && readerFault == null) {
      call.release();
    }
  }
}
