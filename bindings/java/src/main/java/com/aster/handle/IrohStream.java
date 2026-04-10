package com.aster.handle;

import com.aster.ffi.*;
import java.io.IOException;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.concurrent.*;
import java.util.concurrent.Flow.*;
import java.util.concurrent.atomic.AtomicLong;

public class IrohStream implements AutoCloseable {

  private final IrohRuntime runtime;
  private final long handle;
  private volatile boolean closed = false;

  private final MethodHandle streamWrite;
  private final MethodHandle streamFinish;
  private final MethodHandle streamRead;
  private final MethodHandle streamStop;

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

  IrohStream(IrohRuntime runtime, long handle) {
    this(runtime, handle, 0);
  }

  IrohStream(IrohRuntime runtime, long handle, long ignoredRelated) {
    this.runtime = runtime;
    this.handle = handle;

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

    registerEventHandler();
  }

  private void registerEventHandler() {
    runtime.addInboundHandler(
        event -> {
          if (event.handle() != handle) return;

          switch (event.kind()) {
            case IrohEventKind.FRAME_RECEIVED -> {
              if (event.hasBuffer() && event.data() != MemorySegment.NULL && event.dataLen() > 0) {
                byte[] payload = event.data().toArray(ValueLayout.JAVA_BYTE);
                runtime.releaseBuffer(event.buffer());
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
              if (event.kind() == IrohEventKind.STREAM_RESET) {
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
    return handle;
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
    var alloc = lib.allocator();

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
          (int) streamWrite.invoke(runtime.nativeHandle(), handle, payloadSeg, messageId, opSeg);
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
    var alloc = lib.allocator();
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = (int) streamFinish.invoke(runtime.nativeHandle(), handle, 0L, opSeg);
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
      int status = (int) streamStop.invoke(runtime.nativeHandle(), handle, (long) errorCode);
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
    var alloc = lib.allocator();
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = (int) streamRead.invoke(runtime.nativeHandle(), handle, maxLen, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_stream_read failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_stream_read threw: " + t.getMessage());
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    CompletableFuture<byte[]> readFuture = new CompletableFuture<>();
    runtime.registry().register(opId); // error completion only; data via FRAME_RECEIVED
    return readFuture;
  }

  /**
   * Returns a {@link Publisher} of frames received on this stream. The stream closes the publisher
   * when {@code STREAM_FINISHED} or {@code STREAM_RESET} is received.
   */
  public Publisher<byte[]> receiveFrames() {
    return frames;
  }

  public void close() {
    if (!closed) {
      closed = true;
      frames.close();
    }
  }
}
