package site.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutionException;

/**
 * Java FFM wrapper for the Aster reactor C FFI ({@code aster_reactor_*}).
 *
 * <p>The reactor delivers fully-read RPC calls from the Rust accept loop to a Java consumer thread
 * via a lock-free SPSC ring buffer. Each call carries pointers to header and request payload bytes
 * that remain alive until {@link #bufferRelease(long)} is called for each buffer ID.
 *
 * <p>Typical usage from a single poll thread:
 *
 * <pre>{@code
 * Reactor reactor = new Reactor(runtimeHandle, nodeHandle, 256);
 * try (Arena arena = Arena.ofConfined()) {
 *   MemorySegment calls = arena.allocate(Reactor.CALL_LAYOUT, 32);
 *   while (running) {
 *     int n = reactor.poll(calls, 32, 100);
 *     for (int i = 0; i < n; i++) {
 *       MemorySegment c = calls.asSlice(i * Reactor.CALL_LAYOUT.byteSize(), Reactor.CALL_LAYOUT);
 *       long callId = c.get(ValueLayout.JAVA_LONG, 0);
 *       // ... read header/request, dispatch, build response ...
 *       reactor.submit(callId, responseBytes, trailerBytes);
 *       reactor.bufferRelease(c.get(ValueLayout.JAVA_LONG, OFFSET_HEADER_BUFFER));
 *       reactor.bufferRelease(c.get(ValueLayout.JAVA_LONG, OFFSET_REQUEST_BUFFER));
 *       reactor.bufferRelease(c.get(ValueLayout.JAVA_LONG, OFFSET_PEER_BUFFER));
 *     }
 *   }
 * } finally {
 *   reactor.close();
 * }
 * }</pre>
 */
public final class Reactor implements AutoCloseable {

  // ============================================================================
  // aster_reactor_call_t struct layout (matches ffi/src/reactor.rs)
  // ============================================================================

  /**
   * Memory layout of {@code aster_reactor_call_t}. 104 bytes total.
   *
   * <p>Multiplexed-streams update (spec §6/§7.5):
   *
   * <ul>
   *   <li>{@code event_kind} discriminates {@link #EVENT_KIND_CALL} vs {@link
   *       #EVENT_KIND_CONNECTION_CLOSED}. ConnectionClosed events populate only {@code event_kind},
   *       {@code connection_id}, and {@code peer_*}; all other fields are zero/NULL.
   *   <li>{@code connection_id} (new) — used together with {@code StreamHeader.sessionId} to key
   *       per-session state. The same id is reused across every call from one connection and
   *       emitted again on the terminal ConnectionClosed event so the binding can drop session
   *       state for that connection.
   *   <li>{@code is_session_call} (removed) — the binding now decodes {@code sessionId} from the
   *       {@code StreamHeader} payload.
   * </ul>
   */
  public static final MemoryLayout CALL_LAYOUT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_BYTE.withName("event_kind"), //              0
          MemoryLayout.paddingLayout(7), //                             1
          ValueLayout.JAVA_LONG.withName("call_id"), //                 8
          ValueLayout.JAVA_LONG.withName("connection_id"), //          16
          ValueLayout.JAVA_LONG.withName("stream_id"), //              24
          ValueLayout.ADDRESS.withName("header_ptr"), //               32
          ValueLayout.JAVA_INT.withName("header_len"), //              40
          ValueLayout.JAVA_BYTE.withName("header_flags"), //           44
          MemoryLayout.paddingLayout(3), //                            45
          ValueLayout.ADDRESS.withName("request_ptr"), //              48
          ValueLayout.JAVA_INT.withName("request_len"), //             56
          ValueLayout.JAVA_BYTE.withName("request_flags"), //          60
          MemoryLayout.paddingLayout(3), //                            61
          ValueLayout.ADDRESS.withName("peer_ptr"), //                 64
          ValueLayout.JAVA_INT.withName("peer_len"), //                72
          MemoryLayout.paddingLayout(4), //                            76
          ValueLayout.JAVA_LONG.withName("header_buffer"), //          80
          ValueLayout.JAVA_LONG.withName("request_buffer"), //         88
          ValueLayout.JAVA_LONG.withName("peer_buffer"), //            96
          ValueLayout.JAVA_LONG.withName("connection_handle") //      104
          );

  public static final long OFFSET_EVENT_KIND = 0;
  public static final long OFFSET_CALL_ID = 8;
  public static final long OFFSET_CONNECTION_ID = 16;
  public static final long OFFSET_STREAM_ID = 24;
  public static final long OFFSET_HEADER_PTR = 32;
  public static final long OFFSET_HEADER_LEN = 40;
  public static final long OFFSET_HEADER_FLAGS = 44;
  public static final long OFFSET_REQUEST_PTR = 48;
  public static final long OFFSET_REQUEST_LEN = 56;
  public static final long OFFSET_REQUEST_FLAGS = 60;
  public static final long OFFSET_PEER_PTR = 64;
  public static final long OFFSET_PEER_LEN = 72;
  public static final long OFFSET_HEADER_BUFFER = 80;
  public static final long OFFSET_REQUEST_BUFFER = 88;
  public static final long OFFSET_PEER_BUFFER = 96;
  public static final long OFFSET_CONNECTION_HANDLE = 104;

  /** Event-kind constants matching {@code ASTER_EVENT_KIND_*} in {@code ffi/iroh_ffi.h}. */
  public static final byte EVENT_KIND_CALL = 0;

  public static final byte EVENT_KIND_CONNECTION_CLOSED = 1;

  /** A held native channel permit is ready; retry the exact response submit. */
  public static final byte EVENT_KIND_RESPONSE_WRITABLE = 2;

  /** The call closed while native code was waiting to reserve response capacity. */
  public static final byte EVENT_KIND_RESPONSE_CLOSED = 3;

  // ============================================================================
  // Method handles
  // ============================================================================

  private static final MethodHandle CREATE =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_create",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, // status
                  ValueLayout.JAVA_LONG, // runtime
                  ValueLayout.JAVA_LONG, // node
                  ValueLayout.JAVA_INT, // ring_capacity
                  ValueLayout.ADDRESS // out_reactor
                  ));

  private static final MethodHandle DESTROY =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_destroy",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));

  private static final MethodHandle POLL =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_poll",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, // count returned
                  ValueLayout.JAVA_LONG, // runtime
                  ValueLayout.JAVA_LONG, // reactor
                  ValueLayout.ADDRESS, // out_calls
                  ValueLayout.JAVA_INT, // max_calls
                  ValueLayout.JAVA_INT // timeout_ms
                  ));

  private static final MethodHandle SUBMIT =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_submit",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, // status
                  ValueLayout.JAVA_LONG, // runtime
                  ValueLayout.JAVA_LONG, // reactor
                  ValueLayout.JAVA_LONG, // call_id
                  ValueLayout.ADDRESS, // response_ptr
                  ValueLayout.JAVA_INT, // response_len
                  ValueLayout.ADDRESS, // trailer_ptr
                  ValueLayout.JAVA_INT // trailer_len
                  ));

  private static final MethodHandle SUBMIT_FRAME =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_submit_frame",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, // status
                  ValueLayout.JAVA_LONG, // runtime
                  ValueLayout.JAVA_LONG, // reactor
                  ValueLayout.JAVA_LONG, // call_id
                  ValueLayout.ADDRESS, // frame_ptr
                  ValueLayout.JAVA_INT // frame_len
                  ));

  private static final MethodHandle SUBMIT_TRAILER =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_submit_trailer",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, // status
                  ValueLayout.JAVA_LONG, // runtime
                  ValueLayout.JAVA_LONG, // reactor
                  ValueLayout.JAVA_LONG, // call_id
                  ValueLayout.ADDRESS, // trailer_ptr
                  ValueLayout.JAVA_INT // trailer_len
                  ));

  private static final MethodHandle RESERVE_RESPONSE_CAPACITY =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_reserve_response_capacity",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, // status
                  ValueLayout.JAVA_LONG, // runtime
                  ValueLayout.JAVA_LONG, // reactor
                  ValueLayout.JAVA_LONG, // call_id
                  ValueLayout.JAVA_LONG // encoded_len
                  ));

  private static final MethodHandle CANCEL_RESPONSE_RESERVATION =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_cancel_response_reservation",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, // status
                  ValueLayout.JAVA_LONG, // runtime
                  ValueLayout.JAVA_LONG, // reactor
                  ValueLayout.JAVA_LONG // call_id
                  ));

  private static final MethodHandle BUFFER_RELEASE =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_buffer_release",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_LONG));

  private static final MethodHandle CHECK_CANCELLED =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_check_cancelled",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, // 0=alive, 1=cancelled, NOT_FOUND(2)=gone, other=error
                  ValueLayout.JAVA_LONG, // runtime
                  ValueLayout.JAVA_LONG, // reactor
                  ValueLayout.JAVA_LONG // call_id
                  ));

  private static final MethodHandle RECV_FRAME =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_recv_frame",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, // return: status code (0/1/2 = ok/eos/timeout, <0 = err)
                  ValueLayout.JAVA_LONG, // runtime
                  ValueLayout.JAVA_LONG, // reactor
                  ValueLayout.JAVA_LONG, // call_id
                  ValueLayout.JAVA_INT, // timeout_ms
                  ValueLayout.ADDRESS, // out_payload_ptr (**u8)
                  ValueLayout.ADDRESS, // out_payload_len (*u32)
                  ValueLayout.ADDRESS, // out_flags (*u8)
                  ValueLayout.ADDRESS // out_buffer_id (*u64)
                  ));

  /** Status codes returned by {@code aster_reactor_recv_frame}. */
  public static final int RECV_FRAME_OK = 0;

  public static final int RECV_FRAME_END_OF_STREAM = 1;
  public static final int RECV_FRAME_TIMEOUT = 2;

  // ============================================================================
  // Instance state
  // ============================================================================

  private final long runtimeHandle;
  private final long handle;
  private final ConcurrentHashMap<Long, CompletableFuture<Void>> responseCapacityWaiters =
      new ConcurrentHashMap<>();
  private volatile boolean closed = false;

  /**
   * Create a reactor attached to the given node. Starts accepting connections immediately.
   *
   * @param runtimeHandle handle returned from {@code iroh_runtime_new}
   * @param nodeHandle handle of a node created with {@code iroh_node_memory_with_alpns}
   * @param ringCapacity SPSC ring capacity (rounded up to next power of two; 0 → 256)
   * @throws IrohException if the underlying call returns a non-OK status
   */
  public Reactor(long runtimeHandle, long nodeHandle, int ringCapacity) {
    this.runtimeHandle = runtimeHandle;
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment outReactor = arena.allocate(ValueLayout.JAVA_LONG);
      int status = (int) CREATE.invoke(runtimeHandle, nodeHandle, ringCapacity, outReactor);
      checkStatus(status, "aster_reactor_create");
      this.handle = outReactor.get(ValueLayout.JAVA_LONG, 0);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  private static void checkStatus(int code, String op) {
    if (code != IrohStatus.OK.code) {
      IrohStatus status = IrohStatus.fromCode(code);
      throw IrohException.forStatus(status, op + " failed: " + status.name());
    }
  }

  /** Native handle for this reactor. */
  public long handle() {
    return handle;
  }

  /**
   * Drain up to {@code maxCalls} fully-read calls into {@code outCalls}.
   *
   * <p>{@code outCalls} must be a contiguous block large enough to hold {@code maxCalls *
   * CALL_LAYOUT.byteSize()} bytes.
   *
   * @param outCalls caller-provided buffer for {@link #CALL_LAYOUT}-sized descriptors
   * @param maxCalls maximum number of calls to drain
   * @param timeoutMs 0 = non-blocking; otherwise wait up to this duration for at least one call
   * @return number of calls actually written
   */
  public int poll(MemorySegment outCalls, int maxCalls, int timeoutMs) {
    try {
      return (int) POLL.invoke(runtimeHandle, handle, outCalls, maxCalls, timeoutMs);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * Submit a response for a call previously delivered by {@link #poll}.
   *
   * @param callId the call ID from the {@code aster_reactor_call_t}
   * @param responseFrame response bytes (may be empty)
   * @param trailerFrame trailer bytes (may be empty)
   */
  public void submit(long callId, MemorySegment responseFrame, MemorySegment trailerFrame) {
    try {
      MemorySegment respPtr = responseFrame == null ? MemorySegment.NULL : responseFrame;
      int respLen = responseFrame == null ? 0 : (int) responseFrame.byteSize();
      MemorySegment trailerPtr = trailerFrame == null ? MemorySegment.NULL : trailerFrame;
      int trailerLen = trailerFrame == null ? 0 : (int) trailerFrame.byteSize();
      retryWhileQueueFull(
          callId,
          (long) respLen + trailerLen,
          "aster_reactor_submit",
          () ->
              (int)
                  SUBMIT.invoke(
                      runtimeHandle, handle, callId, respPtr, respLen, trailerPtr, trailerLen));
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /** One native submission attempt, returning an {@code iroh_status_t} code. */
  @FunctionalInterface
  interface SubmitAttempt {
    int invoke() throws Throwable;
  }

  /** Wait until native code owns response capacity for the exact retry. */
  @FunctionalInterface
  interface CapacityWait {
    void await() throws Throwable;
  }

  /**
   * Invoke a native submit call, retrying while the reactor's response channel is full.
   *
   * <p>The response channel is bounded, so a peer that stops reading eventually fills it. Native
   * code never blocks on that — it reserves capacity first and returns {@code QUEUE_FULL} without
   * copying the frame or changing any call state, so the same {@code MemorySegment} stays valid and
   * the retry is free. Blocking inside the FFI call instead would pin this virtual thread's
   * carrier, which is exactly the scalability problem {@code CallExecutor} exists to avoid.
   *
   * <p>Waiting therefore happens <em>outside</em> the downcall. Native code asynchronously reserves
   * the exact byte charge and one item slot, then reports that combined permit through the reactor
   * poll ring. The Java producer waits on a {@link CompletableFuture}, which parks a virtual thread
   * without pinning its carrier. The exact retry consumes the held permit, so it cannot race back
   * to {@code QUEUE_FULL}. A cancelled call completes the same waiter as closed, and thread
   * interruption cancels the native reservation before returning to the caller.
   */
  private void retryWhileQueueFull(long callId, long encodedLen, String op, SubmitAttempt attempt)
      throws Throwable {
    retryQueueFull(op, attempt, () -> awaitResponseCapacity(callId, encodedLen, op));
  }

  /** Package-private so the Java retry state machine has a deterministic unit test. */
  static void retryQueueFull(String op, SubmitAttempt attempt, CapacityWait capacityWait)
      throws Throwable {
    while (true) {
      int status = attempt.invoke();
      if (status != IrohStatus.QUEUE_FULL.code) {
        checkStatus(status, op);
        return;
      }
      QUEUE_FULL_HITS.increment();
      capacityWait.await();
    }
  }

  private void awaitResponseCapacity(long callId, long encodedLen, String op) throws Throwable {
    CompletableFuture<Void> waiter = new CompletableFuture<>();
    if (responseCapacityWaiters.putIfAbsent(callId, waiter) != null) {
      throw new IllegalStateException(
          "concurrent response-capacity waits violate the one-producer contract for call "
              + callId);
    }

    int status;
    try {
      status = (int) RESERVE_RESPONSE_CAPACITY.invoke(runtimeHandle, handle, callId, encodedLen);
    } catch (Throwable t) {
      responseCapacityWaiters.remove(callId, waiter);
      throw t;
    }
    if (status != IrohStatus.OK.code) {
      responseCapacityWaiters.remove(callId, waiter);
      checkStatus(status, "aster_reactor_reserve_response_capacity");
    }

    try {
      waiter.get();
    } catch (InterruptedException e) {
      responseCapacityWaiters.remove(callId, waiter);
      int cancelStatus = (int) CANCEL_RESPONSE_RESERVATION.invoke(runtimeHandle, handle, callId);
      if (cancelStatus != IrohStatus.OK.code && cancelStatus != IrohStatus.NOT_FOUND.code) {
        checkStatus(cancelStatus, "aster_reactor_cancel_response_reservation");
      }
      Thread.currentThread().interrupt();
      throw IrohException.forStatus(
          IrohStatus.CANCELLED, op + " interrupted while the response channel was full");
    } catch (ExecutionException e) {
      throw e.getCause();
    }
  }

  /** Reset the diagnostic counter before a benchmark trial. */
  public static void resetQueueFullStats() {
    QUEUE_FULL_HITS.reset();
  }

  private static final java.util.concurrent.atomic.LongAdder QUEUE_FULL_HITS =
      new java.util.concurrent.atomic.LongAdder();

  /** Diagnostic: total {@code QUEUE_FULL} results observed in this JVM. */
  public static long queueFullHits() {
    return QUEUE_FULL_HITS.sum();
  }

  /** Complete one capacity notification delivered by the server's reactor poll loop. */
  public void handleResponseCapacityEvent(byte eventKind, long callId) {
    CompletableFuture<Void> waiter = responseCapacityWaiters.remove(callId);
    if (waiter == null) {
      return;
    }
    if (eventKind == EVENT_KIND_RESPONSE_WRITABLE) {
      waiter.complete(null);
    } else if (eventKind == EVENT_KIND_RESPONSE_CLOSED) {
      waiter.completeExceptionally(
          IrohException.forStatus(
              IrohStatus.NOT_FOUND, "response channel closed for call " + callId));
    } else {
      waiter.completeExceptionally(
          new IllegalArgumentException("not a response-capacity event: " + eventKind));
    }
  }

  /**
   * Submit one response frame for a streaming call. May be called multiple times per call — the
   * call stays open until {@link #submitTrailer} closes it.
   *
   * @param callId the call ID from the {@code aster_reactor_call_t}
   * @param frame already-framed bytes ({@code [4B LE len][1B flags][payload]})
   */
  public void submitFrame(long callId, MemorySegment frame) {
    try {
      MemorySegment ptr = frame == null ? MemorySegment.NULL : frame;
      int len = frame == null ? 0 : (int) frame.byteSize();
      retryWhileQueueFull(
          callId,
          len,
          "aster_reactor_submit_frame",
          () -> (int) SUBMIT_FRAME.invoke(runtimeHandle, handle, callId, ptr, len));
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * Submit the trailer for a call and close the stream. After this call the {@code callId} is no
   * longer valid.
   *
   * @param callId the call ID from the {@code aster_reactor_call_t}
   * @param trailer already-framed trailer bytes (may be empty)
   */
  public void submitTrailer(long callId, MemorySegment trailer) {
    try {
      MemorySegment ptr = trailer == null ? MemorySegment.NULL : trailer;
      int len = trailer == null ? 0 : (int) trailer.byteSize();
      retryWhileQueueFull(
          callId,
          len,
          "aster_reactor_submit_trailer",
          () -> (int) SUBMIT_TRAILER.invoke(runtimeHandle, handle, callId, ptr, len));
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * Release a buffer ID obtained from a call descriptor (header_buffer, request_buffer,
   * peer_buffer). Each buffer ID must be released exactly once.
   */
  public void bufferRelease(long bufferId) {
    try {
      int status = (int) BUFFER_RELEASE.invoke(runtimeHandle, handle, bufferId);
      // NOT_FOUND is not a fatal error here — it just means the buffer was already released
      // or never existed. We swallow it to keep release idempotent at the caller level.
      if (status != IrohStatus.OK.code && status != IrohStatus.NOT_FOUND.code) {
        checkStatus(status, "aster_reactor_buffer_release");
      }
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * Result of a {@link #recvFrame(long, int)} call. Three terminal cases:
   *
   * <ul>
   *   <li>{@link Ok} — a request frame is available; {@code payload} holds the bytes (already
   *       copied out of the native buffer; the buffer has been released)
   *   <li>{@link EndOfStream} — the per-call request channel has been closed by the peer ({@code
   *       FLAG_END_STREAM} or QUIC EOF) or already drained; the binding should stop calling {@code
   *       recvFrame} for this call_id
   *   <li>{@link Timeout} — no frame arrived within {@code timeoutMs}; safe to retry
   * </ul>
   */
  public sealed interface RecvFrame permits RecvFrame.Ok, RecvFrame.EndOfStream, RecvFrame.Timeout {
    record Ok(byte[] payload, byte flags) implements RecvFrame {}

    record EndOfStream() implements RecvFrame {
      public static final EndOfStream INSTANCE = new EndOfStream();
    }

    record Timeout() implements RecvFrame {
      public static final Timeout INSTANCE = new Timeout();
    }
  }

  /**
   * Pull the next ADDITIONAL request frame for a client-streaming or bidi-streaming call. The first
   * request frame is delivered inline via {@link #poll}; this method is for SUBSEQUENT frames only.
   *
   * <p>Blocks up to {@code timeoutMs} waiting for a frame. {@code timeoutMs == 0} is a non-blocking
   * try-recv. The returned payload bytes are copied out of the native buffer registry and the
   * buffer is released before this method returns, so the caller does NOT need to call {@link
   * #bufferRelease} for the result.
   *
   * @return one of {@link RecvFrame.Ok}, {@link RecvFrame.EndOfStream}, {@link RecvFrame.Timeout}
   * @throws IrohException for transport errors (NOT for end-of-stream or timeout, which are normal
   *     terminal states surfaced via the result)
   */
  public RecvFrame recvFrame(long callId, int timeoutMs) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment outPayloadPtr = arena.allocate(ValueLayout.ADDRESS);
      MemorySegment outPayloadLen = arena.allocate(ValueLayout.JAVA_INT);
      MemorySegment outFlags = arena.allocate(ValueLayout.JAVA_BYTE);
      MemorySegment outBufferId = arena.allocate(ValueLayout.JAVA_LONG);

      int status =
          (int)
              RECV_FRAME.invoke(
                  runtimeHandle,
                  handle,
                  callId,
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

        // Copy the payload out of the native buffer. The buffer registry
        // owns the storage; we release it here so the caller doesn't have
        // to track a bufferId.
        byte[] payload =
            payloadLen == 0
                ? new byte[0]
                : payloadAddr.reinterpret(payloadLen).toArray(ValueLayout.JAVA_BYTE);
        bufferRelease(bufferId);
        return new RecvFrame.Ok(payload, flags);
      }
      if (status == RECV_FRAME_END_OF_STREAM) {
        return RecvFrame.EndOfStream.INSTANCE;
      }
      if (status == RECV_FRAME_TIMEOUT) {
        return RecvFrame.Timeout.INSTANCE;
      }
      // NOT_FOUND from the FFI layer (call_id unknown or already drained)
      // is also a terminal end-of-stream from the binding's perspective.
      if (status == IrohStatus.NOT_FOUND.code) {
        return RecvFrame.EndOfStream.INSTANCE;
      }
      checkStatus(status, "aster_reactor_recv_frame");
      return RecvFrame.EndOfStream.INSTANCE; // unreachable
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * Whether a long-running emit loop for this call should stop.
   *
   * <p>Returns {@code true} when the call has been cancelled by the peer <em>or</em> when the call
   * id is unknown — already completed, or cancelled and reaped. Both mean "stop"; neither means
   * "keep going".
   *
   * <p>This used to be {@code return status == 1}, which read {@code IROH_STATUS_NOT_FOUND} —
   * positive code 2 — as {@code false} and told the producer to continue streaming on a call that
   * no longer existed. Since cancellation now reaps per-call state as soon as the response receiver
   * drops, NOT_FOUND is the <em>common</em> answer after a cancel, not an edge case.
   *
   * @return {@code true} if the loop must stop, {@code false} while the call is live
   * @throws IrohException on any other status
   */
  public boolean checkCancelled(long callId) {
    try {
      int status = (int) CHECK_CANCELLED.invoke(runtimeHandle, handle, callId);
      if (status == CHECK_CANCELLED_LIVE) {
        return false;
      }
      if (status == CHECK_CANCELLED_CANCELLED || status == IrohStatus.NOT_FOUND.code) {
        return true;
      }
      // Anything else is a real error and must not be silently read as "live".
      IrohStatus mapped = IrohStatus.fromCode(status);
      throw IrohException.forStatus(
          mapped, "aster_reactor_check_cancelled failed: " + mapped.name());
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /** The call is alive and not cancelled. */
  private static final int CHECK_CANCELLED_LIVE = 0;

  /** The peer sent FLAG_CANCEL, or the stream errored. */
  private static final int CHECK_CANCELLED_CANCELLED = 1;

  /** Destroy the reactor. Idempotent. */
  @Override
  public void close() {
    if (closed) {
      return;
    }
    closed = true;
    IrohException closedError =
        IrohException.forStatus(IrohStatus.NOT_FOUND, "reactor closed during response wait");
    responseCapacityWaiters.forEach((callId, waiter) -> waiter.completeExceptionally(closedError));
    responseCapacityWaiters.clear();
    try {
      DESTROY.invoke(runtimeHandle, handle);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  private static RuntimeException rethrow(Throwable t) {
    if (t instanceof RuntimeException re) {
      return re;
    }
    if (t instanceof Error err) {
      throw err;
    }
    return new RuntimeException(t);
  }
}
