package site.aster.ffi;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;

/** Layouts of the ABI 3.5 structs, the k/v list codec and the QUEUE_FULL retry state machine. */
final class DispatchContractTest {

  private static long offsetOf(MemoryLayout layout, String name) {
    return layout.byteOffset(MemoryLayout.PathElement.groupElement(name));
  }

  @Test
  void eventLayoutMatchesTheCAbi() {
    assertEquals(104, Dispatch.EVENT_LAYOUT.byteSize());
    MemoryLayout l = Dispatch.EVENT_LAYOUT;
    assertEquals(Dispatch.OFFSET_KIND, offsetOf(l, "kind"));
    assertEquals(Dispatch.OFFSET_PATTERN, offsetOf(l, "pattern"));
    assertEquals(Dispatch.OFFSET_REASON, offsetOf(l, "reason"));
    assertEquals(Dispatch.OFFSET_FLAGS, offsetOf(l, "flags"));
    assertEquals(Dispatch.OFFSET_CALL_ID, offsetOf(l, "call_id"));
    assertEquals(Dispatch.OFFSET_SERVICE_IDX, offsetOf(l, "service_idx"));
    assertEquals(Dispatch.OFFSET_METHOD_IDX, offsetOf(l, "method_idx"));
    assertEquals(Dispatch.OFFSET_SESSION_KEY, offsetOf(l, "session_key"));
    assertEquals(Dispatch.OFFSET_CONNECTION_ID, offsetOf(l, "connection_id"));
    assertEquals(Dispatch.OFFSET_CONNECTION_HANDLE, offsetOf(l, "connection_handle"));
    assertEquals(Dispatch.OFFSET_DEADLINE_UNIX_MS, offsetOf(l, "deadline_unix_ms"));
    assertEquals(Dispatch.OFFSET_WIRE_CALL_ID, offsetOf(l, "wire_call_id"));
    assertEquals(Dispatch.OFFSET_SESSION_ID, offsetOf(l, "session_id"));
    assertEquals(Dispatch.OFFSET_REQUEST_PTR, offsetOf(l, "request_ptr"));
    assertEquals(Dispatch.OFFSET_REQUEST_LEN, offsetOf(l, "request_len"));
    assertEquals(Dispatch.OFFSET_PEER_PTR, offsetOf(l, "peer_ptr"));
    assertEquals(Dispatch.OFFSET_PEER_LEN, offsetOf(l, "peer_len"));
    assertEquals(Dispatch.OFFSET_BUFFER, offsetOf(l, "buffer"));
    assertEquals(72, Rpc.OPTIONS_LAYOUT.byteSize());
    assertEquals(56, offsetOf(Rpc.OPTIONS_LAYOUT, "session_id"));
    assertEquals(68, offsetOf(Rpc.OPTIONS_LAYOUT, "wire_call_id"));
  }

  @Test
  void libraryIsAbi7() {
    int v = IrohLibrary.getInstance().abiVersion();
    assertEquals(7, v >> 16);
    assertEquals(IrohLibrary.EXPECTED_ABI_MAJOR, v >> 16);
  }

  @Test
  void kvListRoundTrips() {
    Map<String, String> m = new LinkedHashMap<>();
    m.put("a", "1");
    m.put("", "");
    m.put("ключ", "значение");
    assertEquals(m, Dispatch.decodeKv(Dispatch.encodeKv(m)));
    assertEquals(Map.of(), Dispatch.decodeKv(new byte[0]));
    assertEquals(4, Dispatch.encodeKv(Map.of()).length);
  }

  @Test
  void queueFullLearnsTheLengthWaitsThenRetriesTheSameSubmission() throws Throwable {
    AtomicInteger attempts = new AtomicInteger();
    List<Long> waits = new ArrayList<>();
    List<Boolean> withOut = new ArrayList<>();
    Dispatch.resetQueueFullStats();

    Dispatch.Outcome out =
        Dispatch.retryQueueFull(
            "test",
            req -> {
              int n = attempts.getAndIncrement();
              withOut.add(req != MemorySegment.NULL);
              if (n < 2) {
                if (req != MemorySegment.NULL) {
                  req.set(ValueLayout.JAVA_LONG, 0, 77L);
                }
                return IrohStatus.QUEUE_FULL.code;
              }
              return IrohStatus.OK.code;
            },
            waits::add);

    assertEquals(Dispatch.Outcome.OK, out);
    assertEquals(3, attempts.get());
    assertEquals(List.of(false, true, true), withOut, "the hot path passes no out-pointer");
    assertEquals(List.of(77L), waits, "one wait for the learned length");
    assertEquals(2, Dispatch.queueFullHits());
  }

  @Test
  void aCallThatIsOverIsGoneNotRetried() throws Throwable {
    AtomicInteger attempts = new AtomicInteger();
    for (IrohStatus gone : List.of(IrohStatus.NOT_FOUND, IrohStatus.ALREADY_CLOSED)) {
      assertEquals(
          Dispatch.Outcome.GONE,
          Dispatch.retryQueueFull(
              "test",
              req -> {
                attempts.incrementAndGet();
                return gone.code;
              },
              len -> {
                throw new AssertionError("no wait");
              }));
    }
    assertEquals(2, attempts.get());
    assertThrows(
        IrohException.class,
        () -> Dispatch.retryQueueFull("test", req -> IrohStatus.INVALID_ARGUMENT.code, len -> {}));
  }
}
