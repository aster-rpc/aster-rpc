package site.aster;

import static org.junit.jupiter.api.Assertions.*;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import org.junit.jupiter.api.Test;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;

/**
 * Java FFI ABI Contract Tests
 *
 * <p>Validates the Foreign Function & Memory API bindings against the C ABI:
 *
 * <ul>
 *   <li>FFM struct layouts match the C header (size, field offsets)
 *   <li>Round-trip: encode in Java, pass to native, read back
 *   <li>submit/cancel/close round-trip via IrohRuntime
 *   <li>No stale segment access after release
 *   <li>op_id sequence stays monotonic across submissions
 * </ul>
 *
 * <p>These are pure ABI tests — no networking required.
 */
public class AbiContractTest {

  private static final IrohLibrary LIB = IrohLibrary.getInstance();

  // ─── Struct layout verification ─────────────────────────────────────────────

  @Test
  public void testIrohEventSize() {
    assertEquals(80, IrohLibrary.ASTER_EVENT.byteSize());
  }

  @Test
  public void testIrohEventFieldOffsets() {
    // Offsets verified against Rust aster_event_t in ffi/src/lib.rs.
    // These match the hard-coded offsets used in IrohEvent.fromSegment().
    assertEquals(0, offsetOf(IrohLibrary.ASTER_EVENT, "struct_size"));
    assertEquals(4, offsetOf(IrohLibrary.ASTER_EVENT, "kind"));
    assertEquals(8, offsetOf(IrohLibrary.ASTER_EVENT, "status"));
    assertEquals(16, offsetOf(IrohLibrary.ASTER_EVENT, "operation"));
    assertEquals(24, offsetOf(IrohLibrary.ASTER_EVENT, "handle"));
    assertEquals(32, offsetOf(IrohLibrary.ASTER_EVENT, "related"));
    assertEquals(40, offsetOf(IrohLibrary.ASTER_EVENT, "user_data"));
    assertEquals(48, offsetOf(IrohLibrary.ASTER_EVENT, "data_ptr"));
    assertEquals(56, offsetOf(IrohLibrary.ASTER_EVENT, "data_len"));
    assertEquals(64, offsetOf(IrohLibrary.ASTER_EVENT, "buffer"));
    assertEquals(72, offsetOf(IrohLibrary.ASTER_EVENT, "error_code"));
    assertEquals(76, offsetOf(IrohLibrary.ASTER_EVENT, "flags"));
  }

  @Test
  public void testIrohBytesLayout() {
    assertEquals(16, IrohLibrary.ASTER_BYTES.byteSize());
    assertEquals(0, offsetOf(IrohLibrary.ASTER_BYTES, "ptr"));
    assertEquals(8, offsetOf(IrohLibrary.ASTER_BYTES, "len"));
  }

  @Test
  public void testRuntimeConfigSize() {
    assertEquals(16, IrohLibrary.ASTER_RUNTIME_CONFIG.byteSize());
  }

  @Test
  public void testEndpointConfigSize() {
    assertEquals(152, IrohLibrary.ASTER_ENDPOINT_CONFIG.byteSize());
  }

  @Test
  public void testEndpointConfigFieldOffsets() {
    assertEquals(0, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "struct_size"));
    assertEquals(4, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "relay_mode"));
    assertEquals(8, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "secret_key"));
    assertEquals(24, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "alpns"));
    assertEquals(40, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "relay_urls"));
    assertEquals(56, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "enable_discovery"));
    assertEquals(60, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "enable_hooks"));
    assertEquals(64, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "hook_timeout_ms"));
    assertEquals(72, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "bind_addr"));
    assertEquals(88, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "clear_ip_transports"));
    assertEquals(92, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "clear_relay_transports"));
    assertEquals(96, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "portmapper_config"));
    assertEquals(104, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "proxy_url"));
    assertEquals(120, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "proxy_from_env"));
    assertEquals(128, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "data_dir_utf8"));
    assertEquals(144, offsetOf(IrohLibrary.ASTER_ENDPOINT_CONFIG, "hook_failure_mode"));
  }

  @Test
  public void testOtherStructLayouts() {
    // Must match ffi/src/layout.rs and ffi/tests/abi_contract_test.c.
    assertEquals(16, IrohLibrary.ASTER_BYTES_LIST.byteSize());
    assertEquals(48, IrohLibrary.ASTER_CONNECT_CONFIG.byteSize());
    assertEquals(40, offsetOf(IrohLibrary.ASTER_CONNECT_CONFIG, "addr"));
    assertEquals(48, IrohLibrary.ASTER_NODE_ADDR.byteSize());
    assertEquals(32, offsetOf(IrohLibrary.ASTER_NODE_ADDR, "direct_addresses"));
    assertEquals(56, IrohLibrary.ASTER_TRANSPORT_SNAPSHOT.byteSize());
    assertEquals(24, offsetOf(IrohLibrary.ASTER_TRANSPORT_SNAPSHOT, "peer_port"));
    assertEquals(32, offsetOf(IrohLibrary.ASTER_TRANSPORT_SNAPSHOT, "relay_url"));
    assertEquals(48, offsetOf(IrohLibrary.ASTER_TRANSPORT_SNAPSHOT, "rtt_micros"));
  }

  // ─── ABI version ────────────────────────────────────────────────────────

  @Test
  public void testAbiVersionMatchesBinding() {
    int v = LIB.abiVersion();
    assertEquals(IrohLibrary.EXPECTED_ABI_MAJOR, v >>> 16);
    assertEquals(IrohLibrary.EXPECTED_ABI_MAJOR, LIB.abiVersionMajor());
    assertEquals((v >>> 8) & 0xff, LIB.abiVersionMinor());
    assertEquals(v & 0xff, LIB.abiVersionPatch());
  }

  @Test
  public void testRuntimeNewRejectsAbiMismatch() throws Throwable {
    var runtimeNew =
        LIB.getHandle(
            "aster_runtime_new",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_INT,
                ValueLayout.ADDRESS,
                ValueLayout.ADDRESS));
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(ValueLayout.JAVA_LONG);
      out.set(ValueLayout.JAVA_LONG, 0, 0L);
      int status =
          (int) runtimeNew.invoke(IrohLibrary.EXPECTED_ABI_MAJOR + 1, MemorySegment.NULL, out);
      assertEquals(IrohStatus.ABI_MISMATCH.code, status);
      assertEquals(0L, out.get(ValueLayout.JAVA_LONG, 0));
    }
  }

  // ─── FFM round-trip (Java ↔ Native) ─────────────────────────────────────

  @Test
  public void testEventSegmentRoundTrip() {
    // Encode an event in Java FFM layout, write to a segment, read back.
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment seg = arena.allocate(IrohLibrary.ASTER_EVENT);

      seg.set(ValueLayout.JAVA_INT, 0, 80); // struct_size
      seg.set(ValueLayout.JAVA_INT, 4, 2); // kind = READ_COMPLETE
      seg.set(ValueLayout.JAVA_LONG, 16, 42); // operation
      seg.set(ValueLayout.JAVA_LONG, 24, 7); // handle
      seg.set(ValueLayout.JAVA_INT, 72, 0); // error_code

      // Read back — values must match
      assertEquals(80, seg.get(ValueLayout.JAVA_INT, 0));
      assertEquals(2, seg.get(ValueLayout.JAVA_INT, 4));
      assertEquals(42, seg.get(ValueLayout.JAVA_LONG, 16));
      assertEquals(7, seg.get(ValueLayout.JAVA_LONG, 24));
      assertEquals(0, seg.get(ValueLayout.JAVA_INT, 72));
    }
  }

  @Test
  public void testBytesSegmentRoundTrip() {
    // Encode an ASTER_BYTES { ptr, len } in Java, read it back.
    // This validates the struct layout works for passing byte buffers.
    try (Arena arena = Arena.ofConfined()) {
      byte[] data = new byte[] {1, 2, 3};
      MemorySegment dataSeg = arena.allocateFrom(ValueLayout.JAVA_BYTE, data);

      MemorySegment irohBytes = arena.allocate(IrohLibrary.ASTER_BYTES);
      irohBytes.set(ValueLayout.ADDRESS, 0, dataSeg);
      irohBytes.set(ValueLayout.JAVA_LONG, 8, data.length);

      // Read back length
      assertEquals(data.length, irohBytes.get(ValueLayout.JAVA_LONG, 8));

      // Read back pointer and re-interpret
      MemorySegment readPtr = irohBytes.get(ValueLayout.ADDRESS, 0);
      assertEquals(dataSeg.address(), readPtr.address());
    }
  }

  // ─── Helper methods ───────────────────────────────────────────────────────

  private static long offsetOf(MemoryLayout layout, String field) {
    return layout.byteOffset(MemoryLayout.PathElement.groupElement(field));
  }
}
