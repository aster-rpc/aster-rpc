package site.aster.client;

import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import site.aster.codec.Codec;
import site.aster.ffi.Rpc;
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
 */
public final class BidiCall<Req, Resp> implements AutoCloseable {

  private static final Object END_SENTINEL = new Object();

  private final long runtime;
  private final long call;
  private final Codec codec;
  private final Class<Resp> responseType;
  private final BlockingQueue<Object> responses = new LinkedBlockingQueue<>();
  private final Thread readerThread;
  private volatile RpcError trailerError;
  private volatile Throwable readerFault;
  private volatile boolean completed = false;
  private volatile boolean cleanTrailer = false;
  private volatile boolean closed = false;

  /** Package-private: {@code call} is an open bidi call ({@code aster_rpc_open}). */
  BidiCall(long runtime, long call, Codec codec, Class<Resp> responseType) {
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
    Rpc.send(runtime, call, codec.encode(request), false);
  }

  /** Signal end-of-request (an empty end-of-stream frame). Safe to call at most once. */
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
      Rpc.cancel(runtime, call);
    }
  }

  /**
   * Block until the next response arrives or the stream ends. Returns {@code null} on a clean OK
   * trailer. Throws {@link RpcError} on a non-OK trailer or transport error.
   */
  public Resp recv() throws InterruptedException {
    return unwrap(responses.take());
  }

  /** Test-only hook: poll for a response with a timeout, returning {@code null} on expiry. */
  Resp recvWithTimeout(long timeout, TimeUnit unit) throws InterruptedException {
    Object item = responses.poll(timeout, unit);
    if (item == null) {
      return null;
    }
    return unwrap(item);
  }

  @SuppressWarnings("unchecked")
  private Resp unwrap(Object item) {
    if (item == END_SENTINEL) {
      responses.offer(END_SENTINEL); // keep later recv() calls terminal
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
        Rpc.Recv r = Rpc.recv(runtime, call, Rpc.TIMEOUT_INFINITE);
        if (r instanceof Rpc.Recv.Data d) {
          responses.offer(codec.decode(d.payload(), responseType));
        } else if (r instanceof Rpc.Recv.Trailer t) {
          RpcError e = AsterClient.statusError(t.status());
          if (e == null) {
            cleanTrailer = true;
          } else {
            trailerError = e;
          }
          responses.offer(END_SENTINEL);
          return;
        }
      }
    } catch (Throwable t) {
      readerFault = t;
      responses.offer(END_SENTINEL);
    }
  }

  /** Close the call: cancels it if it has not ended, then frees it. Idempotent. */
  @Override
  public void close() {
    if (closed) {
      return;
    }
    if (!cleanTrailer && trailerError == null) {
      Rpc.cancel(runtime, call);
    }
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
