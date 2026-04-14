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
 * True interleaved bidi-streaming call. Use when the server emits responses while the client is
 * still sending requests (ping-pong patterns, server-driven backpressure, request-response loops).
 *
 * <p>Wraps an {@link AsterCall} handle from the multiplexed-stream pool: the StreamHeader has
 * already been written by {@link AsterClient#openBidiStream}, and this object drives the
 * request/response frame traffic. On {@link #close}, the underlying stream is released back to the
 * pool (on a clean trailer) or discarded (on any error path).
 *
 * <p>Lifecycle:
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

  private final AsterCall call;
  private final Codec codec;
  private final ForyCodec headerCodec;
  private final Class<Resp> responseType;
  private final BlockingQueue<Object> responses = new LinkedBlockingQueue<>();
  private final Thread readerThread;
  private volatile RpcError trailerError;
  private volatile Throwable readerFault;
  private volatile boolean completed = false;
  private volatile boolean cleanTrailer = false;

  /**
   * Package-private constructor. The caller MUST have already written the StreamHeader frame on
   * {@code call} before handing it over.
   */
  BidiCall(AsterCall call, Codec codec, ForyCodec headerCodec, Class<Resp> responseType) {
    this.call = call;
    this.codec = codec;
    this.headerCodec = headerCodec;
    this.responseType = responseType;
    this.readerThread = Thread.ofVirtual().name("aster-bidi-reader").start(this::readLoop);
  }

  /** Send one request frame. Blocks until the underlying QUIC write completes. */
  public void send(Req request) {
    byte[] reqBytes = codec.encode(request);
    byte[] frame = AsterFraming.encodeFrame(reqBytes, (byte) 0);
    call.sendFrame(frame);
  }

  /**
   * Signal end-of-request. Sends an empty frame with {@link AsterFraming#FLAG_END_STREAM} so the
   * server's reader closes its per-call request channel. Safe to call at most once.
   */
  public void complete() {
    if (completed) return;
    completed = true;
    call.sendFrame(AsterFraming.encodeFrame(new byte[0], AsterFraming.FLAG_END_STREAM));
  }

  /**
   * Cancel the call. Sends a {@code FLAG_CANCEL} frame; the caller should still drain {@link #recv}
   * until it returns {@code null} to pick up any trailing responses + the trailer.
   */
  public void cancel() {
    call.sendFrame(AsterFraming.encodeFrame(new byte[0], AsterFraming.FLAG_CANCEL));
  }

  /**
   * Block until the next response frame arrives or the stream ends. Returns {@code null} on a clean
   * OK trailer. Throws {@link RpcError} on a non-OK trailer or transport error.
   */
  public Resp recv() throws InterruptedException {
    Object item = responses.take();
    return unwrap(item);
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
      readerFault = t;
      responses.offer(END_SENTINEL);
    }
  }

  /**
   * Close the call. Releases the underlying stream back to the pool on a clean trailer; discards it
   * on any error path so the pool slot is freed without poisoning the next call that might reuse
   * the stream. Idempotent.
   */
  @Override
  public void close() {
    if (readerThread.isAlive()) {
      try {
        readerThread.join(2_000);
      } catch (InterruptedException e) {
        Thread.currentThread().interrupt();
      }
    }
    if (cleanTrailer && trailerError == null && readerFault == null) {
      call.release();
    } else {
      call.discard();
    }
  }
}
