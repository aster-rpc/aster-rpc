package site.aster.node;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import site.aster.config.AsterConfig;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.NativeConfig;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.server.AsterServer;

/**
 * ABI 5.2: {@code blob_zstd_level} and {@code reuse_last_port} cross the C ABI and reach core, so
 * Java can turn each one off.
 */
final class BlobZstdAndPortReuseTest {

  /** The compressed blobs ALPN a node serves unless blobZstdLevel is 0. */
  private static final String ZSTD_BLOBS_ALPN = "/iroh-bytes/4+zstd/1";

  @TempDir Path tmp;

  private static AsterConfig.Builder loopback() {
    return AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0");
  }

  private static long offset(String field) {
    return IrohLibrary.ASTER_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement(field));
  }

  @Test
  void layoutAndEncoding() {
    assertEquals(288, IrohLibrary.ASTER_CONFIG.byteSize());
    assertEquals(280, offset("blob_zstd_level"));
    assertEquals(284, offset("reuse_last_port"));

    // {level, reuse} -> {native level, native reuse} -> back.
    Object[][] cases = {
      {null, null, 0, 0, null, null},
      {0, false, -1, 2, 0, false},
      {7, true, 7, 1, 7, true},
      {40, null, 19, 0, 19, null},
    };
    for (Object[] c : cases) {
      AsterConfig cfg =
          AsterConfig.builder().blobZstdLevel((Integer) c[0]).reuseLastPort((Boolean) c[1]).build();
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment seg = NativeConfig.toNative(cfg, List.of(), arena);
        assertEquals(c[2], seg.get(ValueLayout.JAVA_INT, 280));
        assertEquals(c[3], seg.get(ValueLayout.JAVA_INT, 284));
        AsterConfig back = NativeConfig.applyTo(seg, AsterConfig.builder()).build();
        assertEquals(c[4], back.blobZstdLevel());
        assertEquals(c[5], back.reuseLastPort());
      }
    }
    assertThrows(IllegalArgumentException.class, () -> AsterConfig.builder().blobZstdLevel(-1));

    AsterConfig defaults = AsterConfig.builder().applyNodeEnv().build();
    assertEquals(3, defaults.blobZstdLevel(), "aster_config_from_env fills the default level");
    assertEquals(Boolean.TRUE, defaults.reuseLastPort());
  }

  @Test
  void fromFileReadsBothThroughRust() throws Exception {
    Path toml = tmp.resolve("aster.toml");
    Files.writeString(
        toml, "[storage]\nblob_zstd_level = \"off\"\n\n[network]\nreuse_last_port = false\n");
    AsterConfig c = AsterConfig.fromFile(toml);
    assertEquals(0, c.blobZstdLevel());
    assertEquals(Boolean.FALSE, c.reuseLastPort());
    Files.writeString(toml, "[storage]\nblob_zstd_level = 9\n");
    c = AsterConfig.fromFile(toml);
    assertEquals(9, c.blobZstdLevel());
    assertEquals(Boolean.TRUE, c.reuseLastPort());
  }

  /** With the level 0 the node does not serve the zstd blobs ALPN, so the dial fails. */
  @Test
  void blobZstdOffDropsTheZstdAlpn() throws Exception {
    IrohNode client = IrohNode.start(loopback().build()).get(30, TimeUnit.SECONDS);
    IrohEndpoint dialer = IrohEndpoint.ofNode(client.runtime(), client.nodeHandle());
    try {
      for (Integer level : new Integer[] {null, 0}) {
        boolean served = level == null;
        IrohNode server =
            IrohNode.start(loopback().blobZstdLevel(level).build()).get(30, TimeUnit.SECONDS);
        try {
          Throwable failure = null;
          try {
            IrohConnection conn =
                dialer
                    .connectNodeAddrAsync(server.nodeAddr(), ZSTD_BLOBS_ALPN)
                    .get(20, TimeUnit.SECONDS);
            conn.close();
          } catch (ExecutionException e) {
            failure = e.getCause();
          }
          if (served) {
            assertNull(failure, "default level serves the zstd ALPN");
          } else {
            assertTrue(failure != null, "blobZstdLevel(0) still served the zstd ALPN");
          }
        } finally {
          server.close();
        }
      }
    } finally {
      client.close();
    }
  }

  /** A persistent server records bind-ports.json only while port reuse is on. */
  @Test
  void reuseLastPortOffRecordsNoBindPorts() throws Exception {
    for (Boolean reuse : new Boolean[] {null, false}) {
      Path data = tmp.resolve("data-" + reuse);
      AsterConfig cfg = loopback().storagePath(data.toString()).reuseLastPort(reuse).build();
      try (AsterServer server =
          AsterServer.builder().config(cfg).build().get(30, TimeUnit.SECONDS)) {
        assertTrue(server.node() != null);
      }
      assertEquals(
          reuse == null, Files.exists(data.resolve("bind-ports.json")), "reuseLastPort=" + reuse);
    }
  }
}
