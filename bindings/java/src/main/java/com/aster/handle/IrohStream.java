package com.aster.handle;

import com.aster.ffi.*;
import java.io.IOException;
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.util.concurrent.*;
import java.util.concurrent.Flow.*;
import java.util.concurrent.atomic.AtomicLong;

/**
 * A bidirectional iroh stream.
 *
 * <p>Two consumption models are available:
 *
 * <ul>
 *   <li>{@link #receiveFrames()} — push-based {@link Publisher} of all received frames
 *   <li>{@link #readAsync(long)} — single-frame read returning a {@link CompletableFuture}
 * </ul>
 *
 * <p>Both models coexist because they suit different usage patterns: the publisher is ideal for
 * streaming processing pipelines, while {@code readAsync} is useful for request-response patterns
 * layered on top of a stream.
 */
public class IrohStream implements AutoCloseable {

  private final IrohRuntime runtime;
  private final long sendHandle;
  private final long recvHandle;
  private volatile boolean closed = false;

  private final MethodHandle streamWrite;
  private final MethodHandle streamFinish;
  private final MethodHandle streamRead;
  private final MethodHandle streamStop;
  private final MethodHandle sendStreamFree;
  private final MethodHandle recvStreamFree;

  /**
   * Pending send operations, keyed by the application message ID echoed in {@code SEND_COMPLETED}.
   * This is the correlation mechanism — {@code user_data} in the FFI call becomes {@code user_data}
   * in the {@code SEND_COMPLETED} event.
   */
  private final ConcurrentHashMap<Long, CompletableFuture<Void>> pendingSends =
      new ConcurrentHashMap<>();

  private final AtomicLong nextMessageId = new AtomicLong(0);

  /** Frames received on this stream. */
  private final SubmissionPublisher<byte[]> frames = new SubmissionPublisher<>();

  /**
   * Stream termination events. Emitted exactly once when {@code STREAM_FINISHED} or {@code
   * STREAM_RESET} is received.
   */
  private final SubmissionPublisher<StreamTerminated> terminated = new SubmissionPublisher<>();

  /**
   * Pending read operations, keyed by op_id returned from {@code iroh_stream_read}. The {@code
   * FRAME_RECEIVED} event carries the same op_id, which completes the pending future with the frame
   * bytes.
   */
  private final ConcurrentHashMap<Long, CompletableFuture<byte[]>> pendingReads =
      new ConcurrentHashMap<>();

  IrohStream(IrohRuntime runtime, long handle) {
    this(runtime, handle, 0);
  }

  IrohStream(IrohRuntime runtime, long sendHandle, long recvHandle) {
    this.runtime = runtime;
    this.sendHandle = sendHandle;
    this.recvHandle = recvHandle;

    var lib = IrohLibrary.getInstance();

    this.streamWrite =
        lib.getHandle(
            "iroh_stream_write",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG, // runtime
                ValueLayout.JAVA_LONG, // send_stream
                IrohLibrary.IROH_BYTES, // data (iroh_bytes_t struct)
                ValueLayout.JAVA_LONG, // user_data — used as message_id echo
                ValueLayout.ADDRESS)); // out_operation

    this.streamFinish =
        lib.getHandle(
            "iroh_stream_finish",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS));

    this.streamRead =
        lib.getHandle(
            "iroh_stream_read",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS));

    this.streamStop =
        lib.getHandle(
            "iroh_stream_stop",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_INT));

    this.sendStreamFree =
        lib.getHandle(
            "iroh_send_stream_free",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));

    this.recvStreamFree =
        lib.getHandle(
            "iroh_recv_stream_free",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));

    registerEventHandler();
  }

  private void registerEventHandler() {
    runtime.addInboundHandler(
        event -> {
          // FRAME_RECEIVED events arrive on the recv stream handle.
          // SEND_COMPLETED, STREAM_FINISHED, STREAM_RESET arrive on the send stream handle.
          if (event.handle() != sendHandle && event.handle() != recvHandle) return;

          switch (event.kind()) {
            case IrohEventKind.FRAME_RECEIVED -> {
              byte[] payload = null;
              if (event.hasBuffer() && event.data() != MemorySegment.NULL && event.dataLen() > 0) {
                payload = event.data().asSlice(0, event.dataLen()).toArray(ValueLayout.JAVA_BYTE);
                runtime.releaseBuffer(event.buffer());
              }
              // Complete any pending readAsync future keyed by this op_id
              long opId = event.operation();
              CompletableFuture<byte[]> readFuture = pendingReads.remove(opId);
              if (readFuture != null && payload != null) {
                readFuture.complete(payload);
              }
              // Also submit to the frames publisher for Publisher-based consumers
              if (payload != null) {
                frames.submit(payload);
              }
            }
            case IrohEventKind.SEND_COMPLETED -> {
              // user_data echoes the message_id passed to iroh_stream_write
              long msgId = event.userData();
              CompletableFuture<Void> future = pendingSends.remove(msgId);
              if (future != null) {
                future.complete(null);
              }
            }
            case IrohEventKind.STREAM_FINISHED, IrohEventKind.STREAM_RESET -> {
              boolean isReset = event.kind() == IrohEventKind.STREAM_RESET;
              int errorCode = event.errorCode();
              terminated.submit(
                  new StreamTerminated(isReset ? Reason.RESET : Reason.FINISHED, errorCode));
              if (isReset) {
                frames.closeExceptionally(new IOException("stream reset"));
              } else {
                frames.close();
              }
            }
            default -> {}
          }
        });
  }

  public long nativeHandle() {
    return sendHandle;
  }

  /**
   * Send a framed payload over this stream.
   *
   * <p>The {@code message_id} is echoed in the resulting {@code SEND_COMPLETED} event as {@code
   * user_data}, allowing correlation without an operation registry lookup.
   *
   * @param payload the bytes to send
   * @return a future that completes when the send is acknowledged
   */
  public CompletableFuture<Void> sendAsync(byte[] payload) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    long messageId = nextMessageId.incrementAndGet();

    // Build iroh_bytes_t payload struct inline
    var payloadSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    var payloadCopy = alloc.allocate(payload.length);
    payloadCopy.copyFrom(MemorySegment.ofArray(payload));
    payloadSeg.set(ValueLayout.ADDRESS, 0, payloadCopy);
    payloadSeg.set(ValueLayout.JAVA_LONG, 8, (long) payload.length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      // runtime, send_stream, data (iroh_bytes_t), user_data, out_operation
      int status =
          (int)
              streamWrite.invoke(runtime.nativeHandle(), sendHandle, payloadSeg, messageId, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_stream_write failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_stream_write threw: " + t.getMessage());
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);

    // Register the operation to catch async errors (ERROR events)
    runtime.registry().register(opId);

    // Track by message_id so the SEND_COMPLETED handler can complete it
    CompletableFuture<Void> sendFuture = new CompletableFuture<>();
    pendingSends.put(messageId, sendFuture);

    return sendFuture;
  }

  /**
   * Finish this stream's send side — signals no more frames will be sent.
   *
   * @return a future that completes when the finish is acknowledged
   */
  public CompletableFuture<Void> finishAsync() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = (int) streamFinish.invoke(runtime.nativeHandle(), sendHandle, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_stream_finish failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_stream_finish threw: " + t.getMessage());
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime.registry().register(opId).thenApply(e -> null);
  }

  /**
   * Reset the stream with a QUIC error code. Synchronous.
   *
   * @param errorCode the QUIC error code
   */
  public void reset(int errorCode) {
    try {
      int status = (int) streamStop.invoke(runtime.nativeHandle(), sendHandle, (long) errorCode);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_stream_stop failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_stream_stop threw: " + t.getMessage());
    }
  }

  /**
   * Read the next frame from this stream asynchronously.
   *
   * @param maxLen maximum bytes to read
   * @return a future that completes with the received bytes
   */
  public CompletableFuture<byte[]> readAsync(long maxLen) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = (int) streamRead.invoke(runtime.nativeHandle(), sendHandle, maxLen, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_stream_read failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_stream_read threw: " + t.getMessage());
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    CompletableFuture<byte[]> readFuture = new CompletableFuture<>();
    runtime.registry().register(opId);
    // Track so FRAME_RECEIVED handler can complete this future with the frame bytes
    pendingReads.put(opId, readFuture);
    return readFuture;
  }

  /**
   * Returns a {@link Publisher} of frames received on this stream. The stream closes the publisher
   * when {@code STREAM_FINISHED} or {@code STREAM_RESET} is received.
   */
  public Publisher<byte[]> receiveFrames() {
    return frames;
  }

  /**
   * Returns a {@link Publisher} that emits exactly one {@link StreamTerminated} event when this
   * stream is closed, either cleanly ({@code STREAM_FINISHED}) or abruptly ({@code STREAM_RESET}).
   *
   * <p>This publisher closes after emitting the termination event.
   */
  public Publisher<StreamTerminated> closed() {
    return terminated;
  }

  public void close() {
    if (!closed) {
      closed = true;
      frames.close();
      terminated.close();
      // Free both native stream handles. These are synchronous calls — safe to call from
      // any thread. It is safe to call free on a stream that has already been freed by
      // the remote (Rust will return NOT_FOUND which we ignore).
      try {
        sendStreamFree.invoke(runtime.nativeHandle(), sendHandle);
      } catch (Throwable t) {
        System.err.println("iroh_send_stream_free failed: " + t.getMessage());
      }
      if (recvHandle != 0) {
        try {
          recvStreamFree.invoke(runtime.nativeHandle(), recvHandle);
        } catch (Throwable t) {
          System.err.println("iroh_recv_stream_free failed: " + t.getMessage());
        }
      }
    }
  }

  /** Reason a stream was terminated. */
  public enum Reason {
    /** Stream ended cleanly — all data was read and the send side was finished. */
    FINISHED,
    /** Stream was reset abruptly by the remote or local peer. */
    RESET
  }

  /**
   * Emitted by {@link #closed()} when this stream terminates.
   *
   * @param reason why the stream ended
   * @param errorCode QUIC error code (0 if finished cleanly)
   */
  public record StreamTerminated(Reason reason, int errorCode) {}
}
