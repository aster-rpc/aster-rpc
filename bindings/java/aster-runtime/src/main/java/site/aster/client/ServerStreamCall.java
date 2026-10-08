package site.aster.client;

import java.util.concurrent.TimeUnit;
import site.aster.codec.Codec;
import site.aster.ffi.Rpc;
import site.aster.interceptors.InterceptorChain;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;

/**
 * Incremental server-streaming call. The client has already sent its single request and now reads
 * responses one at a time via {@link #recv()} until the trailer.
 *
 * <p>Contrast with {@link AsterClient#callServerStream}, which buffers the whole list and only
 * suits FINITE streams. Use this class for open-ended streams (e.g. a log tail) where the caller
 * stops early.
 *
 * <pre>{@code
 * try (ServerStreamCall<LogEntry> call =
 *     client.openServerStream(addr, "MissionControl", "tailLogs",
 *                             new TailRequest("", "info"), LogEntry.class).get()) {
 *   LogEntry entry = call.recv();           // block for first entry
 *   // ...decide to stop early...
 * }   // close() cancels the call if it has not ended
 * }</pre>
 *
 * <p>Unread buffering is bounded to 256 queued/in-flight items and 8 MiB of queued raw response
 * payload plus at most one legal 16 MiB in-flight frame (24 MiB combined). A single frame larger
 * than 8 MiB is admitted only into an empty buffer. Capacity is acquired before decoding. Decoded
 * object heap size depends on the codec. A stalled consumer backpressures the native reader;
 * cancel/close wakes it.
 */
public final class ServerStreamCall<Resp> implements AutoCloseable {

  private static final Object END_SENTINEL = new Object();

  private final long runtime;
  private final long call;
  private final Codec codec;
  private final Class<Resp> responseType;

  /** The client's interceptors and this call's context; {@code null} when it has none. */
  private final AsterClient.Intercepted intercepted;

  private final ResponseBuffer responses = new ResponseBuffer(END_SENTINEL);
  private final Thread readerThread;
  private volatile RpcError trailerError;
  private volatile Throwable readerFault;
  private volatile boolean cleanTrailer = false;
  private volatile boolean closed = false;

  /** Set by {@link #cancel} / {@link #close}; the reader observes it between bounded waits. */
  private volatile boolean cancelRequested = false;

  /** Package-private: the request has been sent on {@code call} ({@code aster_rpc_open}). */
  ServerStreamCall(
      long runtime,
      long call,
      Codec codec,
      Class<Resp> responseType,
      AsterClient.Intercepted intercepted) {
    this.intercepted = intercepted;
    this.runtime = runtime;
    this.call = call;
    this.codec = codec;
    this.responseType = responseType;
    // The read blocks in native code: a platform thread keeps virtual-thread carriers free.
    this.readerThread =
        Thread.ofPlatform().daemon(true).name("aster-server-stream-reader").start(this::readLoop);
  }

  /**
   * Block until the next response arrives or the stream ends. Returns {@code null} on a clean OK
   * trailer. Throws {@link RpcError} on a non-OK trailer or transport error.
   */
  public Resp recv() throws InterruptedException {
    return deliver(responses.take());
  }

  /** Poll with a timeout: {@code null} on expiry OR clean end ({@link #isComplete()} tells). */
  public Resp recvWithTimeout(long timeout, TimeUnit unit) throws InterruptedException {
    Object item = responses.poll(timeout, unit);
    if (item == null) return null;
    return deliver(item);
  }

  /** Cancel the call ({@code FLAG_CANCEL} then a reset); the reader ends with {@code CANCELLED}. */
  public void cancel() {
    if (!closed) {
      cancelRequested = true;
      responses.cancel();
      Rpc.cancel(runtime, call);
    }
  }

  public boolean isComplete() {
    return cleanTrailer;
  }

  /**
   * {@link #unwrap} through the client's interceptors: the response interceptors on a response, the
   * error interceptors (reverse order) on a failure (one that suppresses it ends the stream).
   */
  @SuppressWarnings("unchecked")
  private Resp deliver(Object item) {
    if (intercepted == null) {
      return unwrap(item);
    }
    Resp response;
    try {
      response = unwrap(item);
    } catch (RpcError e) {
      RpcError surfaced = InterceptorChain.applyError(intercepted.chain(), intercepted.ctx(), e);
      if (surfaced == null) {
        return null;
      }
      throw surfaced;
    }
    return response == null
        ? null
        : (Resp) InterceptorChain.applyResponse(intercepted.chain(), intercepted.ctx(), response);
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
        responses.beginRead();
        Rpc.Recv r = Rpc.recvPolling(runtime, call, () -> cancelRequested);
        if (r instanceof Rpc.Recv.Data d) {
          responses.awaitCapacity(d.payload().length);
          responses.data(codec.decode(d.payload(), responseType), d.payload().length);
        } else if (r instanceof Rpc.Recv.Trailer t) {
          RpcError e = AsterClient.statusError(t.status());
          if (e == null) {
            cleanTrailer = true;
          } else {
            trailerError = e;
          }
          responses.finish();
          return;
        }
      }
    } catch (Throwable t) {
      if (cancelRequested) {
        trailerError = new RpcError(StatusCode.CANCELLED, "call cancelled");
      } else {
        readerFault = t;
      }
      responses.finish();
    }
  }

  /** Close the call: cancels it if it has not ended, then frees it. Idempotent. */
  @Override
  public void close() {
    if (closed) {
      return;
    }
    if (!cleanTrailer && trailerError == null) {
      cancelRequested = true;
      responses.cancel();
      Rpc.cancel(runtime, call);
    }
    responses.cancel();
    if (readerThread.isAlive()) {
      try {
        readerThread.join(2_000);
      } catch (InterruptedException e) {
        Thread.currentThread().interrupt();
      }
    }
    closed = true;
    Rpc.release(runtime, call);
  }
}
