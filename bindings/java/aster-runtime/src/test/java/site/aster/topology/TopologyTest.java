package site.aster.topology;

import static org.junit.jupiter.api.Assertions.*;

import java.lang.foreign.Arena;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.function.BooleanSupplier;
import org.junit.jupiter.api.Test;
import site.aster.config.AsterConfig;
import site.aster.ffi.IrohLibrary;
import site.aster.node.IrohNode;
import site.aster.testing.WaitBudget;
import site.aster.upcall.AdmittedFilterHandle;
import site.aster.upcall.Upcalls;

public final class TopologyTest {
  public static IrohNode node(boolean monitoring) throws Exception {
    return IrohNode.start(
            AsterConfig.builder()
                .relayMode("disabled")
                .bindAddr("127.0.0.1:0")
                .enableMonitoring(monitoring)
                .portmapper(false)
                .build())
        .get(20, TimeUnit.SECONDS);
  }

  static void until(BooleanSupplier condition) throws Exception {
    long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(25);
    while (!condition.getAsBoolean()) {
      if (System.nanoTime() > deadline) fail("topology convergence timeout");
      Thread.sleep(100);
    }
  }

  static void add(IrohNode from, IrohNode to) {
    try (Arena a = Arena.ofConfined()) {
      assertEquals(
          0,
          IrohLibrary.getInstance()
              .addNodeAddr(
                  from.runtime().nativeHandle(), from.nodeHandle(), to.nodeAddr().toNative(a)));
    }
  }

  @Test
  void disabledMonitoringIsEmptyAndSharedRequiresMonitoring() throws Exception {
    var node = node(false);
    try (var rt = node.runtime();
        node) {
      var topology = node.topology();
      assertFalse(topology.hasMonitoring());
      assertEquals(List.of(), topology.peers());
      assertTrue(topology.peer("00".repeat(32)).isEmpty());
      assertThrows(IllegalArgumentException.class, () -> topology.peer("broken"));
      assertThrows(
          Exception.class,
          () -> topology.snapshotAsync().get(WaitBudget.SECONDS, TimeUnit.SECONDS));
      assertThrows(
          Exception.class,
          () ->
              topology
                  .enableSharedAsync(new byte[32], null)
                  .get(WaitBudget.SECONDS, TimeUnit.SECONDS));
    }
  }

  @Test
  void nativeViewsConvergeAndSnapshotRemainsOneObservationAcrossRevocation() throws Exception {
    var a = node(true);
    var b = node(true);
    try (var ar = a.runtime();
        var br = b.runtime();
        a;
        b;
        var up = Upcalls.create(ar);
        var filter = AdmittedFilterHandle.create(up)) {
      until(
          () ->
              !a.nodeAddr().directAddresses().isEmpty()
                  && !b.nodeAddr().directAddresses().isEmpty());
      add(a, b);
      add(b, a);
      String aid = a.nodeId(), bid = b.nodeId();
      filter.set(aid, true);
      filter.set(bid, true);
      var timing = new Topology.Timing(Duration.ZERO, Duration.ofSeconds(60), null, null, 1L, 1L);
      byte[] secret = new byte[32];
      java.util.Arrays.fill(secret, (byte) 0x53);
      a.topology()
          .enableSharedAsync(
              secret,
              new Topology.SwarmOptions(List.of(bid), filter, Duration.ofMillis(200), timing))
          .get(10, TimeUnit.SECONDS);
      b.topology()
          .enableSharedAsync(
              secret, new Topology.SwarmOptions(List.of(aid), null, Duration.ofMillis(200), timing))
          .get(10, TimeUnit.SECONDS);
      until(() -> a.topology().peer(bid).isPresent());
      var peer = a.topology().peer(bid).orElseThrow();
      assertEquals(Topology.LadderLevel.SAME_HOST, peer.level());
      assertTrue(peer.direct());
      assertNotNull(peer.remoteAddr());
      assertTrue(peer.lastMeasuredUnixMs().signum() > 0);
      assertTrue(a.topology().peers().stream().anyMatch(p -> p.nodeId().equals(bid)));
      until(
          () -> {
            try (var s = a.topology().snapshotAsync().join()) {
              return s.clusterOf(aid).map(c -> c.members().size() == 2).orElse(false);
            }
          });
      try (var snapshot = a.topology().snapshotAsync().get(WaitBudget.SECONDS, TimeUnit.SECONDS)) {
        assertEquals(2, snapshot.clusterOf(aid).orElseThrow().members().size());
        assertEquals(Topology.SeparationVerdict.CONNECTED, snapshot.separated(aid, bid));
        assertEquals(Topology.SeparationVerdict.UNKNOWN, snapshot.separated(aid, "00".repeat(32)));
        assertThrows(
            UnsupportedOperationException.class,
            () -> snapshot.clusters().getFirst().members().add("x"));
        filter.set(bid, false);
        until(
            () -> {
              try (var s = a.topology().snapshotAsync().join()) {
                return s.clusterOf(bid).isEmpty();
              }
            });
        assertTrue(snapshot.clusterOf(bid).isPresent());
        assertEquals(Topology.SeparationVerdict.CONNECTED, snapshot.separated(aid, bid));
        assertThrows(IllegalArgumentException.class, () -> snapshot.clusterOf("broken"));
      }
    }
  }

  @Test
  void snapshotCancellationCloseAndRuntimeOwnership() throws Exception {
    var node = node(true);
    try (var rt = node.runtime();
        node) {
      node.topology().enableSharedAsync(new byte[32], null).get(10, TimeUnit.SECONDS);
      for (int i = 0; i < 32; i++) {
        var future = node.topology().snapshotAsync();
        if (!future.cancel(false)) future.get(WaitBudget.SECONDS, TimeUnit.SECONDS).close();
      }
      var snapshot = node.topology().snapshotAsync().get(WaitBudget.SECONDS, TimeUnit.SECONDS);
      assertNotNull(snapshot.clusters());
      snapshot.close();
      snapshot.close();
      assertThrows(IllegalStateException.class, snapshot::clusters);
      var after = node.topology().snapshotAsync().get(WaitBudget.SECONDS, TimeUnit.SECONDS);
      node.close();
      assertNotNull(after.clusters());
      rt.close();
      assertThrows(RuntimeException.class, after::clusters);
      after.close();
    }
  }

  @Test
  void peerParserPreservesNullableAnd64BitMetrics() throws Exception {
    var value =
        Topology.JSON.readTree(
            """
            {"node_id":"id","level":4,"level_reason":"test","rtt_us":4294967297,"jitter_us":null,
             "loss_ppm":3,"throughput_bps":4294967298,"cwnd_ceiling_bps":4294967299,"direct":false,
             "relay_url":null,"remote_addr":null,"congestion_events":4294967300,"samples":4294967301,
             "confidence_ppm":123,"last_measured_unix_ms":4294967302,"cluster_held_since_unix_ms":0,"is_connected":false}
            """);
    var peer = Topology.peer(value);
    assertEquals(new java.math.BigInteger("4294967297"), peer.rttUs());
    assertNull(peer.jitterUs());
    assertEquals(new java.math.BigInteger("4294967298"), peer.throughputBps());
    assertEquals(new java.math.BigInteger("4294967301"), peer.samples());
    ((com.fasterxml.jackson.databind.node.ObjectNode) value)
        .put("samples", new java.math.BigInteger("18446744073709551615"));
    assertEquals(new java.math.BigInteger("18446744073709551615"), Topology.peer(value).samples());
    assertNull(peer.remoteAddr());
    assertEquals(Topology.LadderLevel.FAR, peer.level());
  }

  @Test
  void peerParserReadsQuicTrafficAndBlobTransfer() throws Exception {
    var value =
        Topology.JSON.readTree(
            """
            {"node_id":"id","level":0,"level_reason":"t","rtt_us":null,"jitter_us":null,"loss_ppm":0,
             "throughput_bps":0,"cwnd_ceiling_bps":0,"direct":true,"relay_url":null,"remote_addr":null,
             "congestion_events":0,"samples":0,"confidence_ppm":0,"last_measured_unix_ms":0,
             "cluster_held_since_unix_ms":0,"is_connected":true,
             "quic_traffic":{"blobs":{"quic_bytes_sent":10,"quic_bytes_received":20},
                             "rpc":{"quic_bytes_sent":4294967296,"quic_bytes_received":0}},
             "blob_transfer":{"fetched_payload_bytes":1,"fetched_zstd_payload_bytes":2,
               "fetched_wire_bytes":3,"fetched_plain_bytes":4,"served_payload_bytes":5,
               "served_zstd_payload_bytes":6,"served_wire_bytes":7,"served_plain_bytes":8,
               "plain_fallbacks":9}}
            """);
    var peer = Topology.peer(value);
    assertEquals(new Topology.QuicBytes(10, 20), peer.quicTraffic("blobs"));
    assertEquals(4294967296L, peer.quicTraffic("rpc").quicBytesSent());
    assertEquals(Topology.QuicBytes.ZERO, peer.quicTraffic("gossip"));
    assertEquals(new Topology.PeerBlobStats(1, 2, 3, 4, 5, 6, 7, 8, 9), peer.blobTransfer());
    // A 7.3-shaped peer (no traffic keys) still parses, with zeros.
    ((com.fasterxml.jackson.databind.node.ObjectNode) value).remove("quic_traffic");
    ((com.fasterxml.jackson.databind.node.ObjectNode) value).remove("blob_transfer");
    var old = Topology.peer(value);
    assertTrue(old.quicTraffic().isEmpty());
    assertEquals(Topology.PeerBlobStats.ZERO, old.blobTransfer());
  }

  @Test
  void peerViewCountsBlobTrafficOfARealFetch() throws Exception {
    var a = node(true);
    var b = node(true);
    try (var ar = a.runtime();
        var br = b.runtime();
        a;
        b) {
      until(
          () ->
              !a.nodeAddr().directAddresses().isEmpty()
                  && !b.nodeAddr().directAddresses().isEmpty());
      add(a, b);
      byte[] data = new byte[64 * 1024];
      new java.util.Random(7).nextBytes(data);
      var hash = b.blobs().addBytesAsync(data).get(20, TimeUnit.SECONDS);
      var fetched =
          a.blobs().downloadHashFromAsync(hash, List.of(b.nodeId())).get(30, TimeUnit.SECONDS);
      assertArrayEquals(data, fetched.data());
      String bid = b.nodeId();
      until(
          () ->
              a.topology()
                  .peer(bid)
                  .map(p -> p.blobTransfer().fetchedPayloadBytes() >= data.length)
                  .orElse(false));
      var peer = a.topology().peer(bid).orElseThrow();
      long received =
          peer.quicTraffic("blobs").quicBytesReceived()
              + peer.quicTraffic("blobs-zstd").quicBytesReceived();
      assertTrue(received > 0, peer.quicTraffic().toString());
      var stats = a.blobs().transferStats();
      assertTrue(stats.fetchedPayloadBytes() >= data.length, stats.toString());
      assertTrue(b.blobs().transferStats().servedPayloadBytes() >= data.length);
    }
  }

  @Test
  void optionalTimingPreservesZeroAndRejectsOverflow() {
    assertDoesNotThrow(() -> new Topology.Timing(Duration.ZERO, null, null, null, 0L, 0L));
    assertThrows(
        IllegalArgumentException.class,
        () -> new Topology.Timing(Duration.ofMillis(0x1_0000_0000L), null, null, null, null, null));
    assertThrows(
        IllegalArgumentException.class,
        () -> new Topology.Timing(Duration.ofNanos(-1), null, null, null, null, null));
    assertThrows(
        IllegalArgumentException.class,
        () -> new Topology.Timing(null, null, null, null, 0x1_0000_0000L, null));
  }
}
