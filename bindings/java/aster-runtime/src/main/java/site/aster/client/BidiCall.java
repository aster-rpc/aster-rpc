package site.aster.client;

import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.handle.IrohStream;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.server.AsterFraming;
import site.aster.server.wire.RpcStatus;

/**
 * True interleaved bidi-streaming call. Replaces the buffered shape of {@link
 * AsterClient#callBidiStream(site.aster.node.NodeAddr, String, String, Iterable, Class)} for use
 * cases where the server emits responses while the client is still sending requests (ping-pong
 * patterns, server-driven backpressure, request-response loops).
 *
 * <p>Lifecycle:
 *
 * <pre>{@code
 * BidiCall<Command, CommandResult> call =
 *     client.openBidiStream(addr, "AgentSession", "runCommand", CommandResult.class).get();
 * try (call) {
 *   call.send(new Command("ls"));
 *   CommandResult r1 = call.recv();
 *   call.send(new Command("date"));
 *   CommandResult r2 = call.recv();
 *   call.complete();              // signal end-of-request to the server
 *   CommandResult tail = call.recv();  // returns null on end-of-stream
 * }
 * }</pre>
 *
 * <p>Threading:
 *
 * <ul>
 *   <li>{@link #send(Object)} encodes and writes one request frame on the underlying QUIC stream;
 *       blocks until the write completes (the underlying QUIC stack provides backpressure).
 *   <li>{@link #recv()} blocks until the next response frame arrives or the trailer closes the
 *       stream. End-of-stream is signalled by returning {@code null}.
 *   <li>A dedicated reader thread continuously reads response frames off the QUIC stream and pushes
 *       them onto an internal {@link BlockingQueue}, so {@link #send(Object)} and {@link #recv()}
 *       never block on each other.
 *   <li>{@link #complete()} finishes the QUIC send side. The reactor's per-stream reader task sees
 *       this as EOF on the request side and closes the per-call request channel.
 * </ul>
 *
 * <p>Use the buffered {@link AsterClient#callBidiStream} for simple "send N requests, get N
 * responses" patterns where you don't need true interleaving — it's a smaller API surface and one
 * fewer thread per call.
 */
public final class BidiCall<Req, Resp> implements AutoCloseable {

  private static final Object END_SENTINEL = new Object();

  private final IrohStream stream;
  private final Codec codec;
  private final ForyCodec headerCodec;
  private final Class<Resp> responseType;
  private final BlockingQueue<Object> responses = new LinkedBlockingQueue<>();
  private final Thread readerThread;
  private volatile RpcError trailerError;
  private volatile Throwable readerFault;

  /**
   * Package-private constructor. The stream MUST already have its header frame written — the caller
   * (typically {@link AsterClient#openBidiStream}) is responsible for that as part of its async
   * setup chain so we don't block an executor thread inside this constructor with {@code .get()} on
   * a future that the same thread is supposed to complete.
   */
  BidiCall(IrohStream stream, Codec codec, ForyCodec headerCodec, Class<Resp> responseType) {
    this.stream = stream;
    this.codec = codec;
    this.headerCodec = headerCodec;
    this.responseType = responseType;
    this.readerThread = Thread.ofVirtual().name("aster-bidi-reader").start(this::readLoop);
  }

  /**
   * Send one request frame. Blocks until the underlying QUIC write completes; the write itself is
   * fast (QUIC manages its own send buffer) so callers can typically push frames as fast as they
   * produce them.
   */
  public void send(Req request) throws Exception {
    byte[] reqBytes = codec.encode(request);
    byte[] frame = AsterFraming.encodeFrame(reqBytes, (byte) 0);
    stream.sendAsync(frame).get();
  }

  /**
   * Signal end-of-request to the server. Finishes the QUIC send side, which the reactor's
   * per-stream reader task sees as EOF and uses to close the per-call request channel. After this
   * call, further {@link #send(Object)} calls will fail; further {@link #recv()} calls still drain
   * any pending responses + the trailer.
   */
  public void complete() throws Exception {
    stream.finishAsync().get();
  }

  /**
   * Block until the next response frame arrives or the stream ends. Returns {@code null} on a clean
   * OK trailer. Throws {@link RpcError} on a non-OK trailer or transport error.
   */
  public Resp recv() throws Exception {
    Object item = responses.take();
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
    @SuppressWarnings("unchecked")
    Resp typed = (Resp) item;
    return typed;
  }

  private void readLoop() {
    try {
      ClientFrameReader reader = new ClientFrameReader(stream);
      while (true) {
        ClientFrameReader.Frame frame = reader.readFrame().get();
        byte flags = frame.flags();
        if ((flags & AsterFraming.FLAG_TRAILER) != 0) {
          RpcStatus status =
              frame.payload().length == 0
                  ? RpcStatus.ok()
                  : (RpcStatus) headerCodec.decode(frame.payload(), RpcStatus.class);
          if (status.code() != RpcStatus.OK) {
            trailerError =
                new RpcError(
                    StatusCode.fromValue(status.code()),
                    status.message() == null ? "" : status.message());
          }
          responses.offer(END_SENTINEL);
          return;
        }
        if ((flags & AsterFraming.FLAG_ROW_SCHEMA) != 0) {
          // Skip row schema frames — no row-mode support yet.
          continue;
        }
        Object decoded = codec.decode(frame.payload(), responseType);
        responses.offer(decoded);
      }
    } catch (Throwable t) {
      readerFault = t;
      responses.offer(END_SENTINEL);
    }
  }

  /**
   * Close the bi-stream and stop the reader thread. Idempotent. Safe to call after {@link
   * #complete()}; safe to call without {@link #complete()} (the server will see the stream close as
   * cancellation).
   */
  @Override
  public void close() {
    try {
      stream.close();
    } catch (Exception ignored) {
      // best-effort
    }
    if (readerThread.isAlive()) {
      try {
        readerThread.join(2_000);
      } catch (InterruptedException e) {
        Thread.currentThread().interrupt();
      }
    }
  }

  /** Test-only hook: how long to wait on a single recv before checking shutdown signals. */
  Resp recvWithTimeout(long timeout, TimeUnit unit) throws Exception {
    Object item = responses.poll(timeout, unit);
    if (item == null) {
      return null;
    }
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
    @SuppressWarnings("unchecked")
    Resp typed = (Resp) item;
    return typed;
  }
}
