package com.aster;

import static org.junit.jupiter.api.Assertions.*;

import com.aster.ffi.IrohLibrary;
import com.aster.ffi.IrohStatus;
import com.aster.ffi.Reactor;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import org.junit.jupiter.api.Test;

/**
 * Reactor FFI contract tests.
 *
 * <p>Validates the {@link Reactor} FFM bindings against the native {@code aster_reactor_*} C
 * functions. Includes struct layout checks (matching {@code aster_reactor_call_t} in {@code
 * ffi/src/reactor.rs}) and lifecycle smoke tests.
 */
public class ReactorContractTest {

  // ─── Struct layout verification ───────────────────────────────────────────

  @Test
  public void testCallLayoutSize() {
    // 80 bytes total: see ffi/src/reactor.rs aster_reactor_call_t.
    assertEquals(80, Reactor.CALL_LAYOUT.byteSize());
  }

  @Test
  public void testCallLayoutFieldOffsets() {
    assertEquals(Reactor.OFFSET_CALL_ID, offsetOf(Reactor.CALL_LAYOUT, "call_id"));
    assertEquals(Reactor.OFFSET_HEADER_PTR, offsetOf(Reactor.CALL_LAYOUT, "header_ptr"));
    assertEquals(Reactor.OFFSET_HEADER_LEN, offsetOf(Reactor.CALL_LAYOUT, "header_len"));
    assertEquals(Reactor.OFFSET_HEADER_FLAGS, offsetOf(Reactor.CALL_LAYOUT, "header_flags"));
    assertEquals(Reactor.OFFSET_REQUEST_PTR, offsetOf(Reactor.CALL_LAYOUT, "request_ptr"));
    assertEquals(Reactor.OFFSET_REQUEST_LEN, offsetOf(Reactor.CALL_LAYOUT, "request_len"));
    assertEquals(Reactor.OFFSET_REQUEST_FLAGS, offsetOf(Reactor.CALL_LAYOUT, "request_flags"));
    assertEquals(Reactor.OFFSET_PEER_PTR, offsetOf(Reactor.CALL_LAYOUT, "peer_ptr"));
    assertEquals(Reactor.OFFSET_PEER_LEN, offsetOf(Reactor.CALL_LAYOUT, "peer_len"));
    assertEquals(Reactor.OFFSET_IS_SESSION_CALL, offsetOf(Reactor.CALL_LAYOUT, "is_session_call"));
    assertEquals(Reactor.OFFSET_HEADER_BUFFER, offsetOf(Reactor.CALL_LAYOUT, "header_buffer"));
    assertEquals(Reactor.OFFSET_REQUEST_BUFFER, offsetOf(Reactor.CALL_LAYOUT, "request_buffer"));
    assertEquals(Reactor.OFFSET_PEER_BUFFER, offsetOf(Reactor.CALL_LAYOUT, "peer_buffer"));
  }

  // ─── Lifecycle against native library ─────────────────────────────────────

  @Test
  public void testReactorLifecycle() throws Throwable {
    long runtime = createRuntime();
    long node = createNode(runtime);

    Reactor reactor = new Reactor(runtime, node, 64);
    assertNotEquals(0, reactor.handle(), "reactor handle should be non-zero");

    // Poll empty ring → 0 calls
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment calls = arena.allocate(Reactor.CALL_LAYOUT, 8);
      int n = reactor.poll(calls, 8, 0);
      assertEquals(0, n, "empty reactor should yield zero calls");
    }

    reactor.close();
    closeRuntime(runtime);
  }

  @Test
  public void testReactorCloseIsIdempotent() throws Throwable {
    long runtime = createRuntime();
    long node = createNode(runtime);

    Reactor reactor = new Reactor(runtime, node, 32);
    reactor.close();
    reactor.close(); // second close should be a no-op

    closeRuntime(runtime);
  }

  @Test
  public void testReactorPollWithTimeout() throws Throwable {
    long runtime = createRuntime();
    long node = createNode(runtime);
    Reactor reactor = new Reactor(runtime, node, 32);

    try (Arena arena = Arena.ofConfined()) {
      MemorySegment calls = arena.allocate(Reactor.CALL_LAYOUT, 8);
      long start = System.currentTimeMillis();
      int n = reactor.poll(calls, 8, 50);
      long elapsed = System.currentTimeMillis() - start;
      assertEquals(0, n);
      // Allow some slack but verify the timeout was honoured (not instant)
      assertTrue(
          elapsed >= 30, "poll with 50ms timeout returned in " + elapsed + "ms (expected >= 30)");
    }

    reactor.close();
    closeRuntime(runtime);
  }

  // ─── Helpers ──────────────────────────────────────────────────────────────

  private static long offsetOf(MemoryLayout layout, String field) {
    return layout.byteOffset(MemoryLayout.PathElement.groupElement(field));
  }

  /** Create a fresh runtime via {@code iroh_runtime_new}. */
  private static long createRuntime() throws Throwable {
    var lib = IrohLibrary.getInstance();
    var runtimeNew =
        lib.getHandle(
            "iroh_runtime_new",
            FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment outRuntime = arena.allocate(ValueLayout.JAVA_LONG);
      int status = (int) runtimeNew.invoke(MemorySegment.NULL, outRuntime);
      assertEquals(IrohStatus.OK.code, status, "iroh_runtime_new failed");
      return outRuntime.get(ValueLayout.JAVA_LONG, 0);
    }
  }

  private static void closeRuntime(long runtime) throws Throwable {
    var lib = IrohLibrary.getInstance();
    var runtimeClose =
        lib.getHandle(
            "iroh_runtime_close",
            FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG));
    runtimeClose.invoke(runtime);
  }

  /** Create a memory node via {@code iroh_node_memory_with_alpns} and return its handle. */
  private static long createNode(long runtime) throws Throwable {
    var lib = IrohLibrary.getInstance();

    var nodeMemoryAlpns =
        lib.getHandle(
            "iroh_node_memory_with_alpns",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG, // runtime
                ValueLayout.ADDRESS, // alpns
                ValueLayout.ADDRESS, // alpn_lens
                ValueLayout.JAVA_LONG, // alpn_count
                ValueLayout.JAVA_LONG, // user_data
                ValueLayout.ADDRESS // out_operation
                ));

    try (Arena arena = Arena.ofConfined()) {
      // Build ALPN list with a single "aster" entry.
      byte[] alpn = "aster".getBytes();
      MemorySegment alpnBytes = arena.allocateFrom(ValueLayout.JAVA_BYTE, alpn);
      MemorySegment alpnsArray = arena.allocate(ValueLayout.ADDRESS);
      alpnsArray.set(ValueLayout.ADDRESS, 0, alpnBytes);
      MemorySegment alpnLens = arena.allocate(ValueLayout.JAVA_LONG);
      alpnLens.set(ValueLayout.JAVA_LONG, 0, alpn.length);
      MemorySegment outOp = arena.allocate(ValueLayout.JAVA_LONG);

      int status = (int) nodeMemoryAlpns.invoke(runtime, alpnsArray, alpnLens, 1L, 0L, outOp);
      assertEquals(IrohStatus.OK.code, status, "iroh_node_memory_with_alpns failed");

      // Drain events to wait for NODE_CREATED
      var pollEvents =
          lib.getHandle(
              "iroh_poll_events",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.ADDRESS,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_INT));

      MemorySegment events = arena.allocate(IrohLibrary.IROH_EVENT, 8);
      // Wait up to 1s for node creation
      for (int i = 0; i < 100; i++) {
        long count = (long) pollEvents.invoke(runtime, events, 8L, 5);
        if (count > 0) {
          // Find a NODE_CREATED event (kind = 1)
          for (long j = 0; j < count; j++) {
            int kind =
                events
                    .asSlice(j * IrohLibrary.IROH_EVENT.byteSize(), IrohLibrary.IROH_EVENT)
                    .get(ValueLayout.JAVA_INT, 4);
            if (kind == 1) {
              // First node created is always handle 1.
              return 1L;
            }
          }
        }
        Thread.sleep(10);
      }
      throw new AssertionError("node creation event not received within 1s");
    }
  }
}
