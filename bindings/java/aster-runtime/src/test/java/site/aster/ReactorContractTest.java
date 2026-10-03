package site.aster;

import static org.junit.jupiter.api.Assertions.*;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.ffi.Reactor;

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
    // 112 bytes total: see ffi/src/reactor.rs aster_reactor_call_t.
    // History: 88 → 104 in the multiplexed-streams migration (added
    // event_kind with 7 bytes alignment padding + connection_id; dropped
    // is_session_call, since the binding decodes sessionId from the
    // StreamHeader); then 104 → 112 by adding connection_handle (the
    // borrowed aster_connection_t handle at offset 104).
    assertEquals(112, Reactor.CALL_LAYOUT.byteSize());
  }

  @Test
  public void testCallLayoutFieldOffsets() {
    assertEquals(Reactor.OFFSET_EVENT_KIND, offsetOf(Reactor.CALL_LAYOUT, "event_kind"));
    assertEquals(Reactor.OFFSET_CALL_ID, offsetOf(Reactor.CALL_LAYOUT, "call_id"));
    assertEquals(Reactor.OFFSET_CONNECTION_ID, offsetOf(Reactor.CALL_LAYOUT, "connection_id"));
    assertEquals(Reactor.OFFSET_STREAM_ID, offsetOf(Reactor.CALL_LAYOUT, "stream_id"));
    assertEquals(Reactor.OFFSET_HEADER_PTR, offsetOf(Reactor.CALL_LAYOUT, "header_ptr"));
    assertEquals(Reactor.OFFSET_HEADER_LEN, offsetOf(Reactor.CALL_LAYOUT, "header_len"));
    assertEquals(Reactor.OFFSET_HEADER_FLAGS, offsetOf(Reactor.CALL_LAYOUT, "header_flags"));
    assertEquals(Reactor.OFFSET_REQUEST_PTR, offsetOf(Reactor.CALL_LAYOUT, "request_ptr"));
    assertEquals(Reactor.OFFSET_REQUEST_LEN, offsetOf(Reactor.CALL_LAYOUT, "request_len"));
    assertEquals(Reactor.OFFSET_REQUEST_FLAGS, offsetOf(Reactor.CALL_LAYOUT, "request_flags"));
    assertEquals(Reactor.OFFSET_PEER_PTR, offsetOf(Reactor.CALL_LAYOUT, "peer_ptr"));
    assertEquals(Reactor.OFFSET_PEER_LEN, offsetOf(Reactor.CALL_LAYOUT, "peer_len"));
    assertEquals(Reactor.OFFSET_HEADER_BUFFER, offsetOf(Reactor.CALL_LAYOUT, "header_buffer"));
    assertEquals(Reactor.OFFSET_REQUEST_BUFFER, offsetOf(Reactor.CALL_LAYOUT, "request_buffer"));
    assertEquals(Reactor.OFFSET_PEER_BUFFER, offsetOf(Reactor.CALL_LAYOUT, "peer_buffer"));
    assertEquals(
        Reactor.OFFSET_CONNECTION_HANDLE, offsetOf(Reactor.CALL_LAYOUT, "connection_handle"));
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

  /**
   * {@code checkCancelled} must treat an unknown call id as terminal.
   *
   * <p>The bug this guards against was silent: {@code return status == 1} read {@code NOT_FOUND} --
   * positive code 2 -- as {@code false}, telling a long-running emit loop to keep streaming on a
   * call that had already been cancelled and reaped. Since cancellation now reaps per-call state as
   * soon as the response receiver drops, NOT_FOUND is the <em>common</em> answer after a cancel,
   * not an edge case, so the wrong mapping meant producers routinely ran on.
   *
   * <p>An unknown id is exactly what a reaped call looks like from Java, and needs no peer, so this
   * is deterministic.
   */
  @Test
  public void testCheckCancelledTreatsUnknownCallAsTerminal() throws Throwable {
    long runtime = createRuntime();
    long node = createNode(runtime);
    Reactor reactor = new Reactor(runtime, node, 64);
    try {
      long unknown = 0xDEAD_BEEF_CAFE_F00DL;
      assertTrue(
          reactor.checkCancelled(unknown),
          "an unknown call id means the call is gone -- cancelled and reaped, or already "
              + "completed. Reporting it live tells a producer to stream on forever.");
      // Stable across polls: a reaped call must not flicker back to live.
      for (int i = 0; i < 20; i++) {
        assertTrue(reactor.checkCancelled(unknown), "flipped to live on poll " + i);
      }
    } finally {
      // Release the node and runtime too. Leaking them keeps endpoints and a
      // tokio runtime alive for the rest of the class, and this harness already
      // fails intermittently on a one-second node-creation budget -- adding
      // load to it makes an unrelated test the one that flakes.
      reactor.close();
      closeRuntime(runtime);
    }
  }

  /** NOT_FOUND is positive; the old doc comment claimed error codes were negative. */
  @Test
  public void testNotFoundIsPositive() {
    assertTrue(IrohStatus.NOT_FOUND.code > 0, "NOT_FOUND must be positive");
  }

  /** Create a fresh runtime via {@code aster_runtime_new}. */
  private static long createRuntime() throws Throwable {
    var lib = IrohLibrary.getInstance();
    var runtimeNew =
        lib.getHandle(
            "aster_runtime_new",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_INT,
                ValueLayout.ADDRESS,
                ValueLayout.ADDRESS));
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment outRuntime = arena.allocate(ValueLayout.JAVA_LONG);
      int status =
          (int) runtimeNew.invoke(IrohLibrary.EXPECTED_ABI_MAJOR, MemorySegment.NULL, outRuntime);
      assertEquals(IrohStatus.OK.code, status, "aster_runtime_new failed");
      return outRuntime.get(ValueLayout.JAVA_LONG, 0);
    }
  }

  private static void closeRuntime(long runtime) throws Throwable {
    var lib = IrohLibrary.getInstance();
    var runtimeClose =
        lib.getHandle(
            "aster_runtime_close",
            FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG));
    runtimeClose.invoke(runtime);
  }

  /** Create a memory node via {@code aster_node_memory_with_alpns} and return its handle. */
  private static long createNode(long runtime) throws Throwable {
    var lib = IrohLibrary.getInstance();

    var nodeMemoryAlpns =
        lib.getHandle(
            "aster_node_memory_with_alpns",
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
      assertEquals(IrohStatus.OK.code, status, "aster_node_memory_with_alpns failed");
      long nodeOperation = outOp.get(ValueLayout.JAVA_LONG, 0);

      // Correlate NODE_CREATED with the submitted operation. The former
      // one-second sleep loop was load-sensitive, and returning hard-coded
      // handle 1 made the test observe a handle it had not actually received.
      var pollEvents =
          lib.getHandle(
              "aster_poll_events",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.ADDRESS,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.JAVA_INT));

      MemorySegment events = arena.allocate(IrohLibrary.ASTER_EVENT, 8);
      long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(30);
      while (System.nanoTime() < deadline) {
        long count = (long) pollEvents.invoke(runtime, events, 8L, 250);
        for (long j = 0; j < count; j++) {
          MemorySegment event =
              events.asSlice(j * IrohLibrary.ASTER_EVENT.byteSize(), IrohLibrary.ASTER_EVENT);
          if (event.get(ValueLayout.JAVA_LONG, 16) != nodeOperation) {
            continue;
          }
          assertEquals(
              IrohEventKind.NODE_CREATED.code,
              event.get(ValueLayout.JAVA_INT, 4),
              "node operation event kind");
          assertEquals(
              IrohStatus.OK.code, event.get(ValueLayout.JAVA_INT, 8), "node creation status");
          return event.get(ValueLayout.JAVA_LONG, 24);
        }
      }
      throw new AssertionError(
          "node creation operation " + nodeOperation + " did not complete within 30s");
    }
  }
}
