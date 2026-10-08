package site.aster.client;

import java.util.concurrent.TimeUnit;
import site.aster.codec.Codec;
import site.aster.ffi.Rpc;
import site.aster.interceptors.InterceptorChain;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;

/**
 * True interleaved bidi-streaming call. Use when the server emits responses while the client is
 * still sending requests (ping-pong patterns, request-response loops).
 *
 * <pre>{@code
 * BidiCall<Command, CommandResult> call =
 *     client.openBidiStream(addr, "AgentSession", "runCommand", CommandResult.class).get();
 * try (call) {
 *   call.send(new Command("ls"));
 *   CommandResult r1 = call.recv();
 *   call.complete();                     // signal end-of-request
 *   while (call.recv() != null) { ... }  // drain trailing responses
 * }
 * }</pre>
 *
 * <p>Unread buffering is bounded to 256 queued/in-flight items and 8 MiB of queued raw response
 * payload plus at most one legal 16 MiB in-flight frame (24 MiB combined). A single frame larger
 * than 8 MiB is admitted only into an empty buffer. Capacity is acquired before decoding. Decoded
 * object heap size depends on the codec. A stalled consumer backpressures the native reader;
 * cancel/close wakes it.
 */
public final class BidiCall<Req, Resp> implements AutoCloseable {

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
  private volatile boolean completed = false;
  private volatile boolean cleanTrailer = false;
  private volatile boolean closed = false;

  /** Set by {@link #cancel} / {@link #close}; the reader observes it between bounded waits. */
  private volatile boolean cancelRequested = false;

  /** Package-private: {@code call} is an open bidi call ({@code aster_rpc_open}). */
  BidiCall(
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
    this.readerThread =
        Thread.ofPlatform().daemon(true).name("aster-bidi-reader").start(this::readLoop);
  }

  /**
   * Send one request. Blocks until the QUIC write completes. A call that is already over (failed,
   * cancelled, answered) drops the request; {@link #recv} reports why.
   */
  public void send(Req request) {
    Object outgoing =
        intercepted == null
            ? request
            : InterceptorChain.applyRequest(intercepted.chain(), intercepted.ctx(), request);
    Rpc.send(runtime, call, codec.encode(outgoing), false);
  }

  /**
   * Signal end-of-request (an empty end-of-stream frame). Repeated calls have no effect. Completing
   * before any request cancels the native call; {@link #recv} reports a non-OK error. The
   * independent reader may observe CANCELLED before the native finish validation error is recorded,
   * or INVALID_ARGUMENT afterward. No handler is dispatched for the empty request.
   */
  public void complete() {
    if (completed) return;
    completed = true;
    Rpc.send(runtime, call, new byte[0], true);
  }

  /**
   * Cancel the call ({@code FLAG_CANCEL} then a reset); {@link #recv} ends with {@code CANCELLED}.
   */
  public void cancel() {
    if (!closed) {
      cancelRequested = true;
      responses.cancel();
      Rpc.cancel(runtime, call);
    }
  }

  /**
   * Block until the next response arrives or the stream ends. Returns {@code null} on a clean OK
   * trailer. Throws {@link RpcError} on a non-OK trailer or transport error.
   */
  public Resp recv() throws InterruptedException {
    return deliver(responses.take());
  }

  /** Test-only hook: poll for a response with a timeout, returning {@code null} on expiry. */
  Resp recvWithTimeout(long timeout, TimeUnit unit) throws InterruptedException {
    Object item = responses.poll(timeout, unit);
    if (item == null) {
      return null;
    }
    return deliver(item);
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
