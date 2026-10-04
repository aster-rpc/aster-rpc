package site.aster.node;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HexFormat;
import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import site.aster.config.AsterConfig;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.NativeConfig;

/** ABI 3.2 config, {@code aster_node_start}, Gate 0 and metrics from Java. */
final class NodeConfigTest {

  @TempDir Path tmp;

  private static AsterConfig.Builder loopback() {
    return AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0");
  }

  @Test
  void configLayoutMatchesTheCAbi() {
    assertEquals(NativeConfig.SIZE, IrohLibrary.ASTER_CONFIG.byteSize());
    assertEquals(
        8,
        IrohLibrary.ASTER_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("relay_urls")));
    assertEquals(
        144,
        IrohLibrary.ASTER_CONFIG.byteOffset(
            MemoryLayout.PathElement.groupElement("hook_timeout_ms")));
    assertEquals(
        200,
        IrohLibrary.ASTER_CONFIG.byteOffset(
            MemoryLayout.PathElement.groupElement("transport_max_concurrent_bidi_streams")));
    assertEquals(
        272, IrohLibrary.ASTER_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("storage")));
  }

  @Test
  void javaConfigRoundTripsThroughTheNativeStruct() {
    AsterConfig c =
        AsterConfig.builder()
            .relayUrls(List.of("https://relay.example"))
            .secretKey(new byte[32])
            .secretKeyFile("/tmp/k")
            .storagePath("/tmp/d")
            .gcIntervalMs(1500)
            .bindAddr("127.0.0.1:0")
            .alpn("aster/1".getBytes(StandardCharsets.UTF_8))
            .localDiscovery(true)
            .enableMonitoring(true)
            .enableHooks(true)
            .hookTimeoutMs(750)
            .hookFailureMode("fail_closed")
            .clearIpTransports(true)
            .portmapper(false)
            .proxyUrl("socks5://p:1080")
            .proxyFromEnv(true)
            .transportMaxConcurrentBidiStreams(7L)
            .transportInitialMtu(1200)
            .transportDatagramReceiveBufferSize(0L)
            .transportSendFairness(false)
            .build();
    AsterConfig back;
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment seg = NativeConfig.toNative(c, List.of(), arena);
      back = NativeConfig.applyTo(seg, AsterConfig.builder()).build();
    }
    assertEquals("custom", back.relayMode());
    assertEquals(List.of("https://relay.example"), back.relayUrls());
    assertArrayEquals(new byte[32], back.secretKey());
    assertEquals("/tmp/k", back.secretKeyFile());
    assertEquals("/tmp/d", back.storagePath());
    assertEquals(1500, back.gcIntervalMs());
    assertEquals("127.0.0.1:0", back.bindAddr());
    assertEquals("aster/1", new String(back.alpns().get(0), StandardCharsets.UTF_8));
    assertTrue(back.localDiscovery() && back.enableMonitoring() && back.enableHooks());
    assertEquals(750, back.hookTimeoutMs());
    assertEquals("fail_closed", back.hookFailureMode());
    assertTrue(back.clearIpTransports());
    assertFalse(back.clearRelayTransports());
    assertEquals("disabled", back.portmapperConfig());
    assertEquals("socks5://p:1080", back.proxyUrl());
    assertTrue(back.proxyFromEnv());
    assertEquals(7L, back.transportMaxConcurrentBidiStreams());
    assertEquals(1200, back.transportInitialMtu());
    assertEquals(0L, back.transportDatagramReceiveBufferSize());
    assertEquals(false, back.transportSendFairness());
    assertNull(back.transportSendWindow());
  }

  @Test
  void fromFileReadsNodeKnobsThroughRustAndTrustInJava() throws Exception {
    Path toml = tmp.resolve("aster.toml");
    Files.writeString(
        toml,
        """
        [trust]
        allow_all_consumers = true

        [storage]
        gc_interval_ms = 60000

        [network]
        relay_mode = "disabled"
        bind_addr = "127.0.0.1:0"
        secret_key = "%s"
        hook_failure_mode = "fail_closed"
        transport_keep_alive_interval_ms = 2000
        """
            .formatted("07".repeat(32)));
    AsterConfig c = AsterConfig.fromFile(toml);
    assertTrue(c.allowAllConsumers());
    assertEquals("disabled", c.relayMode());
    assertEquals("127.0.0.1:0", c.bindAddr());
    assertEquals(60000, c.gcIntervalMs());
    assertEquals("fail_closed", c.hookFailureMode());
    assertEquals(2000L, c.transportKeepAliveIntervalMs());
    byte[] want = new byte[32];
    java.util.Arrays.fill(want, (byte) 7);
    assertArrayEquals(want, c.secretKey(), "64-hex secret key parsed by Rust");

    Files.writeString(toml, "[network]\nhook_failure_mode = \"sometimes\"\n");
    assertThrows(IrohException.class, () -> AsterConfig.fromFile(toml));
  }

  @Test
  void nodeStartsFromKeyFileAndExposesMetrics() throws Exception {
    Path keyFile = tmp.resolve("keys/node.key");
    AsterConfig cfg = loopback().secretKeyFile(keyFile.toString()).build();
    IrohNode node = IrohNode.start(cfg).get(30, TimeUnit.SECONDS);
    String id;
    try {
      id = node.nodeId();
      String onDisk = Files.readString(keyFile).trim();
      assertEquals(HexFormat.of().formatHex(node.exportSecretKey()), onDisk);
      assertTrue(node.metricsPrometheus().contains("iroh_send_ipv4"));
      for (String a : node.nodeAddr().directAddresses()) {
        assertTrue(a.startsWith("127.0.0.1:"), a);
      }
    } finally {
      node.close();
    }
    IrohNode again = IrohNode.start(cfg).get(30, TimeUnit.SECONDS);
    try {
      assertEquals(id, again.nodeId(), "same key file, same identity");
    } finally {
      again.close();
    }
  }

  @Test
  void gateAdmitsAndRevokesAndAttachesToANode() throws Exception {
    try (Gate gate = Gate.create()) {
      String peer = "ab".repeat(32);
      assertFalse(gate.isAdmitted(peer));
      gate.admit(peer);
      assertTrue(gate.isAdmitted(peer));
      gate.reject(451, "blocked").trustMode(Gate.TrustMode.PROTECTED);
      gate.admissionAlpns(List.of("my.admission/1".getBytes(StandardCharsets.UTF_8)));
      assertTrue(gate.isAdmitted(peer), "admissions survive setters");
      gate.revoke(peer);
      assertFalse(gate.isAdmitted(peer));
      assertThrows(IrohException.class, () -> gate.admit("not-hex"));

      IrohNode node = IrohNode.start(loopback().build(), List.of(), gate).get(30, TimeUnit.SECONDS);
      try {
        assertTrue(node.hasHooks());
        node.setGate(null);
        node.setGate(gate);
      } finally {
        node.close();
      }
    }
  }
}
