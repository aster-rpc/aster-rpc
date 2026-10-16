package site.aster.config;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
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
import site.aster.ffi.NativeConfig;
import site.aster.identity.IdentityPublisherConfig;
import site.aster.identity.TrustedIssuer;

/**
 * C ABI 7.7 {@code [identity]}, {@code [identity.publisher]} and {@code [remote_config]}: the Java
 * fields round-trip through {@code aster_config_set_identity} / {@code _identity} (96 bytes),
 * sources included; env and TOML resolve through the Rust config; a builder call drops a source.
 */
final class IdentityConfigTest {

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
    assertEquals(96, NativeConfig.IDENTITY_SIZE);
    AsterConfig d = AsterConfig.builder().build();
    assertFalse(d.acceptOnBehalfOf());
    assertTrue(d.trustedIssuers().isEmpty());
    assertNull(d.clockLeewaySecs());
    assertNull(d.identityPublisher());
    assertTrue(d.remoteConfigEnabled());
    assertFalse(d.hasIdentitySettings());
    AsterConfig back = roundTrip(d);
    assertFalse(back.hasIdentitySettings());
    assertNull(back.edgeTokenMaxLifetimeSecs());
  }

  @Test
  void identityRoundTripsThroughTheNativeStorage() {
    AsterConfig c =
        AsterConfig.builder()
            .acceptOnBehalfOf(true)
            .trustedIssuer(
                TrustedIssuer.of("https://idp.test", List.of("notes"))
                    .withRoles("groups", Map.of("readers", "notes.read"))
                    .withMaxTokenLifetimeSecs(600L))
            .clockLeewaySecs(0L)
            .maxKeySetAgeSecs(3600L)
            .identityPublisher(
                IdentityPublisherConfig.of(
                    List.of("https://idp.test"),
                    Map.of("https://idp.test", "https://idp.test/jwks")))
            .remoteConfigEnabled(false)
            .remoteConfigLock(List.of("identity"))
            .build();
    assertTrue(c.hasIdentitySettings());
    AsterConfig back = roundTrip(c);
    assertTrue(back.acceptOnBehalfOf());
    assertEquals(1, back.trustedIssuers().size());
    TrustedIssuer ti = back.trustedIssuers().get(0);
    assertEquals("https://idp.test", ti.iss());
    assertEquals(List.of("notes"), ti.audiences());
    assertEquals("groups", ti.rolesClaim());
    assertEquals(Map.of("readers", "notes.read"), ti.roleMap());
    assertEquals(600L, ti.maxTokenLifetimeSecs());
    assertEquals(0L, back.clockLeewaySecs(), "leeway 0 is a value");
    assertEquals(3600L, back.maxKeySetAgeSecs());
    assertNull(back.edgeTokenMaxLifetimeSecs());
    assertEquals(
        "https://idp.test/jwks", back.identityPublisher().jwksUrl().get("https://idp.test"));
    assertFalse(back.remoteConfigEnabled());
    assertEquals(List.of("identity"), back.remoteConfigLock());
  }

  @Test
  void tomlIdentityResolvesThroughRust() throws Exception {
    Path toml = tmp.resolve("aster.toml");
    Files.writeString(
        toml,
        """
        [identity]
        accept_on_behalf_of = true
        edge_token_max_lifetime_secs = 120

        [[identity.trusted_issuers]]
        iss = "https://idp.test"
        audiences = ["notes"]
        roles_claim = "groups"
        role_map = { readers = "notes.read" }

        [remote_config]
        lock = ["identity"]
        """);
    AsterConfig c = AsterConfig.fromFile(toml);
    assertTrue(c.acceptOnBehalfOf());
    assertEquals(120L, c.edgeTokenMaxLifetimeSecs());
    assertEquals("notes.read", c.trustedIssuers().get(0).roleMap().get("readers"));
    assertEquals(List.of("identity"), c.remoteConfigLock());
    assertTrue(c.remoteConfigEnabled());
    byte[] sources = c.identitySources();
    assertEquals(2, sources[0], "accept_on_behalf_of from the file");
    assertEquals(2, sources[1], "trusted_issuers from the file");
    assertEquals(2, sources[2], "edge_token_max_lifetime_secs from the file");
    // The sources survive the round trip, and a builder call drops one.
    AsterConfig back = roundTrip(c);
    assertArrayEquals(sources, back.identitySources());
  }

  /** {@code ASTER_IDENTITY_*} / {@code ASTER_REMOTE_CONFIG_*} parity, in a child JVM. */
  @Test
  void envIdentityResolvesThroughRust() throws Exception {
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
                "ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF", "true",
                "ASTER_IDENTITY_TRUSTED_ISSUERS",
                    "[{\"iss\":\"https://idp.test\",\"audiences\":[\"notes\"],\"roles_claim\":"
                        + "\"groups\",\"role_map\":{\"readers\":\"notes.read\"}}]",
                "ASTER_IDENTITY_CLOCK_LEEWAY_SECS", "0",
                "ASTER_IDENTITY_PUBLISHER",
                    "{\"issuers\":[\"https://idp.test\"],\"jwks_url\":{\"https://idp.test\":"
                        + "\"https://idp.test/jwks\"},\"refresh_secs\":600}",
                "ASTER_REMOTE_CONFIG_LOCK", "identity"));
    Process p = pb.start();
    String out;
    try (BufferedReader r =
        new BufferedReader(new InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8))) {
      out = r.lines().collect(Collectors.joining("\n"));
    }
    assertTrue(p.waitFor(60, TimeUnit.SECONDS));
    assertEquals(0, p.exitValue(), out);
    String last = out.lines().reduce((a, b) -> b).orElse("");
    assertEquals("true|notes.read|0|null|600|[identity]|true|3,3,0,3,0|0,3,0,3,0", last, out);
  }

  /** Child-JVM entry point for the env test: prints the resolved settings on one line. */
  public static final class EnvProbe {
    public static void main(String[] args) {
      AsterConfig c = AsterConfig.fromEnv();
      // A value changed in code no longer claims to come from the environment.
      AsterConfig changed = roundTrip(roundTripBuilder(c).acceptOnBehalfOf(false).build());
      System.out.println(
          String.join(
              "|",
              Boolean.toString(c.acceptOnBehalfOf()),
              c.trustedIssuers().get(0).roleMap().get("readers"),
              String.valueOf(c.clockLeewaySecs()),
              String.valueOf(c.edgeTokenMaxLifetimeSecs()),
              String.valueOf(c.identityPublisher().refreshSecs()),
              c.remoteConfigLock().toString(),
              Boolean.toString(c.remoteConfigEnabled()),
              join(roundTrip(c).identitySources()),
              join(changed.identitySources())));
    }

    /** A builder holding {@code c}'s settings (through the native round trip). */
    private static AsterConfig.Builder roundTripBuilder(AsterConfig c) {
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment seg = NativeConfig.toNative(c, List.of(), arena);
        try {
          return NativeConfig.applyTo(seg, AsterConfig.builder());
        } finally {
          NativeConfig.release(seg);
        }
      }
    }

    private static String join(byte[] b) {
      StringBuilder s = new StringBuilder();
      for (int i = 0; i < b.length; i++) {
        if (i > 0) s.append(',');
        s.append(b[i]);
      }
      return s.toString();
    }
  }
}
