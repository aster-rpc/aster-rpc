package site.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;

/**
 * Java FFM wrapper for the client-side {@code aster_call_*} per-call FFI family (spec §8). Each
 * {@code AsterCall} owns a multiplexed bi-stream acquired from the per-connection pool; send/recv
 * ops run opaque framed bytes through that stream and {@link #release}/{@link #discard} return the
 * stream to the pool (success) or drop it (failure).
 *
 * <p>The binding is responsible for framing. {@link #sendFrame} expects already-framed bytes
 * ({@code [4B LE len][1B flags][payload]}), matching the contract from {@code ffi/src/call.rs}.
 *
 * <p>Threading: {@link #sendFrame} and {@link #recvFrame} may be called concurrently from different
 * threads (bidi patterns); {@link #release}/{@link #discard} MUST NOT run while either is in
 * flight.
 */
public final class AsterCall implements AutoCloseable {

  /** Status codes returned by {@code aster_call_recv_frame}. */
  public static final int RECV_FRAME_OK = 0;

  public static final int RECV_FRAME_END_OF_STREAM = 1;
  public static final int RECV_FRAME_TIMEOUT = 2;

  /** Acquire-error subcodes (negative) returned by {@code aster_call_acquire}. */
  public static final int ERR_POOL_FULL = -10;

  public static final int ERR_QUIC_LIMIT_REACHED = -11;
  public static final int ERR_PEER_STREAM_LIMIT_TOO_LOW = -12;
  public static final int ERR_STREAM_OPEN_FAILED = -13;
  public static final int ERR_POOL_CLOSED = -14;

  private static final MethodHandle ACQUIRE =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_call_acquire",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, // return: status or negative subcode
                  ValueLayout.JAVA_LONG, // runtime
                  ValueLayout.JAVA_LONG, // connection
                  ValueLayout.JAVA_INT, // session_id (u32; Java int is fine)
                  ValueLayout.ADDRESS // out_call (aster_call_t*)
                  ));

  private static final MethodHandle SEND_FRAME =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_call_send_frame",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.ADDRESS,
                  ValueLayout.JAVA_INT));

  private static final MethodHandle RECV_FRAME =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_call_recv_frame",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_INT, // timeout_ms
                  ValueLayout.ADDRESS, // out_payload_ptr (**u8)
                  ValueLayout.ADDRESS, // out_payload_len (*u32)
                  ValueLayout.ADDRESS, // out_flags (*u8)
                  ValueLayout.ADDRESS // out_buffer_id (*u64)
                  ));

  private static final MethodHandle RELEASE =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_call_release",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));

  private static final MethodHandle DISCARD =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_call_discard",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));

  private static final MethodHandle BUFFER_RELEASE =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_call_buffer_release",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));

  private final long runtimeHandle;
  private final long callHandle;
  private volatile boolean finished = false;

  private AsterCall(long runtimeHandle, long callHandle) {
    this.runtimeHandle = runtimeHandle;
    this.callHandle = callHandle;
  }

  /**
   * Acquire a call handle from the given connection's multiplexed stream pool.
   *
   * @param runtimeHandle the {@code iroh_runtime_t} handle
   * @param connectionHandle the {@code iroh_connection_t} handle
   * @param sessionId session id (0 = SHARED pool; non-zero = session-bound pool)
   * @throws StreamAcquireException mapped from {@code ASTER_CALL_ERR_*} subcodes
   * @throws IrohException mapped from generic {@code iroh_status_t} codes
   */
  public static AsterCall acquire(long runtimeHandle, long connectionHandle, int sessionId) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment outCall = arena.allocate(ValueLayout.JAVA_LONG);
      int status = (int) ACQUIRE.invoke(runtimeHandle, connectionHandle, sessionId, outCall);
      if (status == IrohStatus.OK.code) {
        return new AsterCall(runtimeHandle, outCall.get(ValueLayout.JAVA_LONG, 0));
      }
      throw mapAcquireError(status);
    } catch (RuntimeException | Error e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException("aster_call_acquire threw: " + t.getMessage());
    }
  }

  private static RuntimeException mapAcquireError(int status) {
    return switch (status) {
      case ERR_POOL_FULL ->
          new StreamAcquireException(StreamAcquireException.Reason.POOL_FULL, "pool full");
      case ERR_QUIC_LIMIT_REACHED ->
          new StreamAcquireException(
              StreamAcquireException.Reason.QUIC_LIMIT_REACHED, "QUIC stream limit reached");
      case ERR_PEER_STREAM_LIMIT_TOO_LOW ->
          new StreamAcquireException(
              StreamAcquireException.Reason.PEER_STREAM_LIMIT_TOO_LOW,
              "peer negotiated max_concurrent_streams below minimum");
      case ERR_STREAM_OPEN_FAILED ->
          new StreamAcquireException(
              StreamAcquireException.Reason.STREAM_OPEN_FAILED, "stream open failed");
      case ERR_POOL_CLOSED ->
          new StreamAcquireException(StreamAcquireException.Reason.POOL_CLOSED, "pool closed");
      default -> {
        IrohStatus irohStatus = IrohStatus.fromCode(status);
        yield new IrohException(irohStatus, "aster_call_acquire failed: " + irohStatus.name());
      }
    };
  }

  /** The native {@code aster_call_t} handle. Exposed for FFI-adjacent helpers; do not close. */
  public long handle() {
    return callHandle;
  }

  /**
   * Push one already-framed request frame on the call's send side. Blocks on the FFI thread until
   * the underlying QUIC {@code write_all} completes.
   *
   * @param frame {@code [4B LE len][1B flags][payload]} bytes
   */
  public void sendFrame(byte[] frame) {
    if (frame == null || frame.length == 0) {
      return;
    }
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment ptr = arena.allocate(frame.length);
      MemorySegment.copy(frame, 0, ptr, ValueLayout.JAVA_BYTE, 0, frame.length);
      int status = (int) SEND_FRAME.invoke(runtimeHandle, callHandle, ptr, frame.length);
      if (status != IrohStatus.OK.code) {
        IrohStatus irohStatus = IrohStatus.fromCode(status);
        throw new IrohException(irohStatus, "aster_call_send_frame failed: " + irohStatus.name());
      }
    } catch (RuntimeException | Error e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException("aster_call_send_frame threw: " + t.getMessage());
    }
  }

  /**
   * Pull the next response frame on the call's recv side. Blocks up to {@code timeoutMs} waiting
   * for a frame. {@code timeoutMs == 0} blocks indefinitely (matches the FFI contract).
   */
  public RecvFrame recvFrame(int timeoutMs) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment outPayloadPtr = arena.allocate(ValueLayout.ADDRESS);
      MemorySegment outPayloadLen = arena.allocate(ValueLayout.JAVA_INT);
      MemorySegment outFlags = arena.allocate(ValueLayout.JAVA_BYTE);
      MemorySegment outBufferId = arena.allocate(ValueLayout.JAVA_LONG);

      int status =
          (int)
              RECV_FRAME.invoke(
                  runtimeHandle,
                  callHandle,
                  timeoutMs,
                  outPayloadPtr,
                  outPayloadLen,
                  outFlags,
                  outBufferId);

      if (status == RECV_FRAME_OK) {
        MemorySegment payloadAddr = outPayloadPtr.get(ValueLayout.ADDRESS, 0);
        int payloadLen = outPayloadLen.get(ValueLayout.JAVA_INT, 0);
        byte flags = outFlags.get(ValueLayout.JAVA_BYTE, 0);
        long bufferId = outBufferId.get(ValueLayout.JAVA_LONG, 0);
        byte[] payload =
            payloadLen == 0
                ? new byte[0]
                : payloadAddr.reinterpret(payloadLen).toArray(ValueLayout.JAVA_BYTE);
        releaseBuffer(bufferId);
        return new RecvFrame.Ok(payload, flags);
      }
      if (status == RECV_FRAME_END_OF_STREAM || status == IrohStatus.NOT_FOUND.code) {
        return RecvFrame.EndOfStream.INSTANCE;
      }
      if (status == RECV_FRAME_TIMEOUT) {
        return RecvFrame.Timeout.INSTANCE;
      }
      IrohStatus irohStatus = IrohStatus.fromCode(status);
      throw new IrohException(irohStatus, "aster_call_recv_frame failed: " + irohStatus.name());
    } catch (RuntimeException | Error e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException("aster_call_recv_frame threw: " + t.getMessage());
    }
  }

  private void releaseBuffer(long bufferId) {
    try {
      int status = (int) BUFFER_RELEASE.invoke(runtimeHandle, bufferId);
      if (status != IrohStatus.OK.code && status != IrohStatus.NOT_FOUND.code) {
        IrohStatus irohStatus = IrohStatus.fromCode(status);
        throw new IrohException(
            irohStatus, "aster_call_buffer_release failed: " + irohStatus.name());
      }
    } catch (RuntimeException | Error e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException("aster_call_buffer_release threw: " + t.getMessage());
    }
  }

  /**
   * Release the call on the success path. The underlying multiplexed stream returns to the pool
   * (LIFO) for reuse. Idempotent.
   */
  public void release() {
    if (finished) {
      return;
    }
    finished = true;
    try {
      RELEASE.invoke(runtimeHandle, callHandle);
    } catch (Throwable ignored) {
      // best-effort
    }
  }

  /**
   * Discard the call on the error path. The underlying multiplexed stream is dropped and any
   * blocked waiter is woken to either reuse a freed slot or surface the same transport error.
   * Idempotent.
   */
  public void discard() {
    if (finished) {
      return;
    }
    finished = true;
    try {
      DISCARD.invoke(runtimeHandle, callHandle);
    } catch (Throwable ignored) {
      // best-effort
    }
  }

  /** Equivalent to {@link #discard()}. Use try-with-resources to ensure cleanup. */
  @Override
  public void close() {
    discard();
  }

  /** Result of a {@link #recvFrame(int)} call. */
  public sealed interface RecvFrame permits RecvFrame.Ok, RecvFrame.EndOfStream, RecvFrame.Timeout {
    record Ok(byte[] payload, byte flags) implements RecvFrame {}

    record EndOfStream() implements RecvFrame {
      public static final EndOfStream INSTANCE = new EndOfStream();
    }

    record Timeout() implements RecvFrame {
      public static final Timeout INSTANCE = new Timeout();
    }
  }
}
