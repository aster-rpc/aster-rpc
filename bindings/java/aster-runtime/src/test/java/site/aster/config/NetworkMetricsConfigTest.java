package site.aster.config;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import site.aster.ffi.IrohException;
import site.aster.ffi.NativeConfig;
import site.aster.node.IrohNode;

/**
 * C ABI 7.3 cluster network settings and 7.4 {@code [metrics]} settings: the Java fields round-trip
 * through {@code aster_config_set_network} / {@code _network} and {@code aster_config_set_metrics}
 * / {@code _metrics}, env and TOML resolve through the Rust config, and a started node applies
 * them.
 */
final class NetworkMetricsConfigTest {

  @TempDir Path tmp;

  private static AsterConfig roundTrip(AsterConfig c) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment seg = NativeConfig.toNative(c, List.of(), arena);
      try {
        return NativeConfig.applyTo(seg, AsterConfig.builder()).build();
      } finally {
        NativeConfig.release(seg);
      }
    }
  }

  @Test
  void defaults() {
    AsterConfig d = AsterConfig.builder().build();
    assertEquals(AsterConfig.DISCOVERY_DEFAULT, d.discovery());
    assertNull(d.bindAddrV6());
    assertTrue(d.advertiseAddrs().isEmpty());
    assertFalse(d.relayCaOnly());
    assertFalse(d.hasNetworkSettings());
    assertFalse(d.metricsPeerLabels());
    assertEquals(64, d.metricsPeerLabelCap());
    AsterConfig back = roundTrip(d);
    assertEquals(AsterConfig.DISCOVERY_DEFAULT, back.discovery());
    assertFalse(back.hasNetworkSettings());
    assertEquals(64, back.metricsPeerLabelCap());
  }

  @Test
  void networkAndMetricsRoundTripThroughTheNativeStorage() {
    AsterConfig c =
        AsterConfig.builder()
            .bindAddrV6("[::]:7842")
            .advertiseAddrs(List.of("203.0.113.10:31842"))
            .relayCaCerts(List.of("/etc/aster/relay-ca.pem"))
            .relayCaOnly(true)
            .discovery("NONE")
            .metricsPeerLabels(true)
            .metricsPeerLabelCap(3)
            .build();
    assertTrue(c.hasNetworkSettings());
    AsterConfig back = roundTrip(c);
    assertEquals("[::]:7842", back.bindAddrV6());
    assertEquals(List.of("203.0.113.10:31842"), back.advertiseAddrs());
    assertEquals(List.of("/etc/aster/relay-ca.pem"), back.relayCaCerts());
    assertTrue(back.relayCaOnly());
    assertEquals(AsterConfig.DISCOVERY_NONE, back.discovery());
    assertTrue(back.metricsPeerLabels());
    assertEquals(3, back.metricsPeerLabelCap());

    // Servers alone select custom discovery, as in Rust.
    AsterConfig custom =
        roundTrip(
            AsterConfig.builder()
                .discoveryPkarrRelays(List.of("https://dns.example.internal/pkarr"))
                .build());
    assertEquals(AsterConfig.DISCOVERY_CUSTOM, custom.discovery());
    assertEquals(List.of("https://dns.example.internal/pkarr"), custom.discoveryPkarrRelays());
  }

  @Test
  void invalidSettingsAreRefused() {
    assertThrows(IllegalArgumentException.class, () -> AsterConfig.builder().discovery("mdns"));
    assertThrows(
        IllegalArgumentException.class, () -> AsterConfig.builder().metricsPeerLabelCap(-1));
    AsterConfig bad =
        AsterConfig.builder()
            .discovery(AsterConfig.DISCOVERY_NONE)
            .discoveryDnsOrigin("dns.example.internal")
            .build();
    assertThrows(IrohException.class, () -> roundTrip(bad));
  }

  @Test
  void tomlNetworkAndMetricsResolveThroughRust() throws Exception {
    Path toml = tmp.resolve("aster.toml");
    Files.writeString(
        toml,
        """
        [network]
        bind_addr_v6 = "[::]:7843"
        advertise_addrs = ["203.0.113.10:31843"]
        relay_ca_only = true
        relay_ca_certs = ["/etc/aster/relay-ca.pem"]
        discovery_dns_origin = "dns.example.internal"
        discovery_pkarr_relays = ["https://dns.example.internal/pkarr"]

        [metrics]
        peer_labels = true
        peer_label_cap = 7
        """);
    AsterConfig c = AsterConfig.fromFile(toml);
    assertEquals("[::]:7843", c.bindAddrV6());
    assertEquals(List.of("203.0.113.10:31843"), c.advertiseAddrs());
    assertTrue(c.relayCaOnly());
    assertEquals(List.of("/etc/aster/relay-ca.pem"), c.relayCaCerts());
    assertEquals(AsterConfig.DISCOVERY_CUSTOM, c.discovery());
    assertEquals("dns.example.internal", c.discoveryDnsOrigin());
    assertEquals(List.of("https://dns.example.internal/pkarr"), c.discoveryPkarrRelays());
    assertTrue(c.metricsPeerLabels());
    assertEquals(7, c.metricsPeerLabelCap());
  }

  /** {@code ASTER_*} env parity, in a child JVM (the env of this one cannot be changed). */
  @Test
  void envNetworkAndMetricsResolveThroughRust() throws Exception {
    String java = Path.of(System.getProperty("java.home"), "bin", "java").toString();
    ProcessBuilder pb =
        new ProcessBuilder(
                java,
                "--enable-native-access=ALL-UNNAMED",
                "-cp",
                System.getProperty("java.class.path"),
                EnvProbe.class.getName())
            .redirectErrorStream(true);
    String lib = System.getProperty("aster.ffi.lib");
    if (lib != null && !lib.isEmpty()) {
      pb.environment().put("IROH_LIB_PATH", lib);
    }
    pb.environment()
        .putAll(
            Map.of(
                "ASTER_BIND_ADDR_V6", "[::]:7842",
                "ASTER_ADVERTISE_ADDRS", "203.0.113.10:31842, 198.51.100.7:31842",
                "ASTER_RELAY_CA_CERTS", "/etc/aster/relay-ca.pem",
                "ASTER_RELAY_CA_ONLY", "1",
                "ASTER_DISCOVERY", "none",
                "ASTER_METRICS_PEER_LABELS", "true",
                "ASTER_METRICS_PEER_LABEL_CAP", "5"));
    Process p = pb.start();
    String out;
    try (BufferedReader r =
        new BufferedReader(new InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8))) {
      out = r.lines().collect(Collectors.joining("\n"));
    }
    assertTrue(p.waitFor(60, TimeUnit.SECONDS));
    assertEquals(0, p.exitValue(), out);
    String last = out.lines().reduce((a, b) -> b).orElse("");
    assertEquals(
        "[::]:7842|[203.0.113.10:31842, 198.51.100.7:31842]|[/etc/aster/relay-ca.pem]|true|none"
            + "|true|5",
        last,
        out);
  }

  /** Child-JVM entry point for the env test: prints the resolved settings on one line. */
  public static final class EnvProbe {
    public static void main(String[] args) {
      AsterConfig c = AsterConfig.fromEnv();
      System.out.println(
          String.join(
              "|",
              String.valueOf(c.bindAddrV6()),
              c.advertiseAddrs().toString(),
              c.relayCaCerts().toString(),
              Boolean.toString(c.relayCaOnly()),
              c.discovery(),
              Boolean.toString(c.metricsPeerLabels()),
              Integer.toString(c.metricsPeerLabelCap())));
    }
  }

  @Test
  void aNodeStartsWithNetworkAndMetricsSettings() throws Exception {
    AsterConfig c =
        AsterConfig.builder()
            .relayMode("disabled")
            .bindAddr("127.0.0.1:0")
            .discovery(AsterConfig.DISCOVERY_NONE)
            .advertiseAddrs(List.of("203.0.113.10:31842"))
            .metricsPeerLabels(true)
            .metricsPeerLabelCap(2)
            .build();
    try (IrohNode node = IrohNode.start(c).get(30, TimeUnit.SECONDS)) {
      assertEquals(64, node.nodeId().length());
    }
  }
}
