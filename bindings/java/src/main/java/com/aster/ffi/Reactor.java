package com.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;

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
   * Memory layout of {@code aster_reactor_call_t}. 80 bytes total with 4-byte alignment padding.
   */
  public static final MemoryLayout CALL_LAYOUT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_LONG.withName("call_id"), //                0
          ValueLayout.ADDRESS.withName("header_ptr"), //               8
          ValueLayout.JAVA_INT.withName("header_len"), //             16
          ValueLayout.JAVA_BYTE.withName("header_flags"), //          20
          MemoryLayout.paddingLayout(3), //                           21
          ValueLayout.ADDRESS.withName("request_ptr"), //             24
          ValueLayout.JAVA_INT.withName("request_len"), //            32
          ValueLayout.JAVA_BYTE.withName("request_flags"), //         36
          MemoryLayout.paddingLayout(3), //                           37
          ValueLayout.ADDRESS.withName("peer_ptr"), //                40
          ValueLayout.JAVA_INT.withName("peer_len"), //               48
          ValueLayout.JAVA_BYTE.withName("is_session_call"), //       52
          MemoryLayout.paddingLayout(3), //                           53
          ValueLayout.JAVA_LONG.withName("header_buffer"), //         56
          ValueLayout.JAVA_LONG.withName("request_buffer"), //        64
          ValueLayout.JAVA_LONG.withName("peer_buffer") //            72
          );

  public static final long OFFSET_CALL_ID = 0;
  public static final long OFFSET_HEADER_PTR = 8;
  public static final long OFFSET_HEADER_LEN = 16;
  public static final long OFFSET_HEADER_FLAGS = 20;
  public static final long OFFSET_REQUEST_PTR = 24;
  public static final long OFFSET_REQUEST_LEN = 32;
  public static final long OFFSET_REQUEST_FLAGS = 36;
  public static final long OFFSET_PEER_PTR = 40;
  public static final long OFFSET_PEER_LEN = 48;
  public static final long OFFSET_IS_SESSION_CALL = 52;
  public static final long OFFSET_HEADER_BUFFER = 56;
  public static final long OFFSET_REQUEST_BUFFER = 64;
  public static final long OFFSET_PEER_BUFFER = 72;

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

  private static final MethodHandle BUFFER_RELEASE =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_reactor_buffer_release",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_LONG));

  // ============================================================================
  // Instance state
  // ============================================================================

  private final long runtimeHandle;
  private final long handle;
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
      int status =
          (int)
              SUBMIT.invoke(
                  runtimeHandle, handle, callId, respPtr, respLen, trailerPtr, trailerLen);
      checkStatus(status, "aster_reactor_submit");
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

  /** Destroy the reactor. Idempotent. */
  @Override
  public void close() {
    if (closed) {
      return;
    }
    closed = true;
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
