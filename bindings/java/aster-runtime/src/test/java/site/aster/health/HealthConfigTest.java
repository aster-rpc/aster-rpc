package site.aster.health;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import site.aster.config.AsterConfig;
import site.aster.node.IrohNode;
import site.aster.server.AsterServer;

/** The ops listener configured from {@code AsterConfig} ({@code [health]}, ASTER_HEALTH_*). */
final class HealthConfigTest {

  @TempDir Path tmp;

  @Test
  void healthIsOffByDefaultAndBindsLocalhost() {
    AsterConfig c = AsterConfig.builder().build();
    assertEquals(0, c.healthPort());
    assertEquals("127.0.0.1", c.healthHost());
    assertNull(c.healthEnabled());
    assertFalse(c.isHealthEnabled());
  }

  @Test
  void portEnablesAndEnabledOverrides() {
    assertTrue(AsterConfig.builder().healthPort(8080).build().isHealthEnabled());
    assertTrue(AsterConfig.builder().healthEnabled(true).build().isHealthEnabled());
    assertFalse(
        AsterConfig.builder().healthPort(8080).healthEnabled(false).build().isHealthEnabled());
    assertThrows(IllegalArgumentException.class, () -> AsterConfig.builder().healthPort(70000));
  }

  @Test
  void tomlHealthSection() throws Exception {
    Path toml = tmp.resolve("aster.toml");
    Files.writeString(toml, "[health]\nport = 9091\nhost = \"10.0.0.5\"\nenabled = false\n");
    AsterConfig c = AsterConfig.builder().applyToml(toml).build();
    assertEquals(9091, c.healthPort());
    assertEquals("10.0.0.5", c.healthHost());
    assertEquals(Boolean.FALSE, c.healthEnabled());
    assertFalse(c.isHealthEnabled());
  }

  @Test
  void serverStartsTheListenerFromConfig() throws Exception {
    AsterConfig cfg =
        AsterConfig.builder()
            .relayMode("disabled")
            .bindAddr("127.0.0.1:0")
            .healthEnabled(true) // port 0 + enabled: an ephemeral port
            .build();
    AsterServer server = AsterServer.builder().config(cfg).build().get(30, TimeUnit.SECONDS);
    try {
      HealthServer health = server.health();
      assertNotNull(health);
      assertTrue(health.address().startsWith("127.0.0.1:"), health.address());
      HttpResponse<String> r =
          HttpClient.newHttpClient()
              .send(
                  HttpRequest.newBuilder(URI.create("http://" + health.address() + "/healthz"))
                      .timeout(Duration.ofSeconds(5))
                      .build(),
                  HttpResponse.BodyHandlers.ofString());
      assertEquals(200, r.statusCode());
      assertTrue(r.body().contains("\"status\":\"ok\""), r.body());
      HttpClient http = HttpClient.newHttpClient();
      String readyUrl = "http://" + health.address() + "/readyz";
      HttpResponse<String> ready =
          http.send(
              HttpRequest.newBuilder(URI.create(readyUrl)).timeout(Duration.ofSeconds(5)).build(),
              HttpResponse.BodyHandlers.ofString());
      assertEquals(200, ready.statusCode(), ready.body());
      var cache = health.check("config-cache");
      cache.set(false);
      HttpResponse<String> failed =
          http.send(
              HttpRequest.newBuilder(URI.create(readyUrl)).timeout(Duration.ofSeconds(5)).build(),
              HttpResponse.BodyHandlers.ofString());
      assertEquals(503, failed.statusCode(), failed.body());
      assertTrue(failed.body().contains("\"config-cache\":false"), failed.body());
      cache.set(true);
      assertEquals(
          200,
          http.send(
                  HttpRequest.newBuilder(URI.create(readyUrl))
                      .timeout(Duration.ofSeconds(5))
                      .build(),
                  HttpResponse.BodyHandlers.ofString())
              .statusCode());
    } finally {
      server.close();
    }
  }

  @Test
  void liveDisabledBindFailureExplicitPrecedenceAndPortReuse() throws Exception {
    try (ServerSocket occupied = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
      int port = occupied.getLocalPort();
      AsterConfig disabled =
          AsterConfig.builder()
              .relayMode("disabled")
              .bindAddr("127.0.0.1:0")
              .healthPort(port)
              .healthEnabled(false)
              .build();
      AsterServer off = AsterServer.builder().config(disabled).build().get(30, TimeUnit.SECONDS);
      try {
        assertNull(off.health());
      } finally {
        off.close();
      }
      AsterConfig conflict =
          AsterConfig.builder()
              .relayMode("disabled")
              .bindAddr("127.0.0.1:0")
              .healthPort(port)
              .healthEnabled(true)
              .build();
      assertThrows(
          java.util.concurrent.ExecutionException.class,
          () -> AsterServer.builder().config(conflict).build().get(30, TimeUnit.SECONDS));
      AsterServer explicit =
          AsterServer.builder()
              .config(conflict)
              .health("127.0.0.1", 0)
              .build()
              .get(30, TimeUnit.SECONDS);
      int actual;
      try {
        assertNotNull(explicit.health());
        actual = Integer.parseInt(explicit.health().address().split(":")[1]);
        assertTrue(actual != port);
        var response =
            HttpClient.newHttpClient()
                .send(
                    HttpRequest.newBuilder(
                            URI.create("http://" + explicit.health().address() + "/readyz"))
                        .build(),
                    HttpResponse.BodyHandlers.ofString());
        assertEquals(200, response.statusCode());
      } finally {
        explicit.close();
      }
      try (ServerSocket rebound = new ServerSocket()) {
        rebound.setReuseAddress(true);
        rebound.bind(new java.net.InetSocketAddress(InetAddress.getLoopbackAddress(), actual));
        assertEquals(actual, rebound.getLocalPort());
      }
    }
  }

  @Test
  void directHealthUsesTheNodeConfiguration() throws Exception {
    AsterConfig cfg =
        AsterConfig.builder()
            .relayMode("disabled")
            .bindAddr("127.0.0.1:0")
            .healthEnabled(true)
            .build();
    try (IrohNode node = IrohNode.start(cfg).get(30, TimeUnit.SECONDS);
        HealthServer health = HealthServer.start(node)) {
      var response =
          HttpClient.newHttpClient()
              .send(
                  HttpRequest.newBuilder(URI.create("http://" + health.address() + "/healthz"))
                      .build(),
                  HttpResponse.BodyHandlers.ofString());
      assertEquals(200, response.statusCode());
    }
  }

  @Test
  void disabledNodeHealthStillAllowsAnExplicitListener() throws Exception {
    AsterConfig cfg =
        AsterConfig.builder()
            .relayMode("disabled")
            .bindAddr("127.0.0.1:0")
            .healthEnabled(false)
            .build();
    try (IrohNode node = IrohNode.start(cfg).get(30, TimeUnit.SECONDS)) {
      assertThrows(IllegalArgumentException.class, () -> HealthServer.start(node));
      try (HealthServer explicit = HealthServer.start(node, "127.0.0.1", 0)) {
        var response =
            HttpClient.newHttpClient()
                .send(
                    HttpRequest.newBuilder(URI.create("http://" + explicit.address() + "/healthz"))
                        .build(),
                    HttpResponse.BodyHandlers.ofString());
        assertEquals(200, response.statusCode());
      }
    }
  }
}
