package site.aster;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import site.aster.blobs.BlobId;
import site.aster.blobs.BlobStatus;
import site.aster.blobs.IrohBlobs;
import site.aster.node.IrohNode;

/**
 * Integration test for blob operations.
 *
 * <p>Note: These tests require the FFI native library to be built and available.
 */
public class BlobIntegrationTest {

  private static final String ALPN = "test-alpn";
  private static final Duration TIMEOUT = Duration.ofSeconds(10);

  @Test
  public void testBlobStatusNonExistent()
      throws ExecutionException, InterruptedException, TimeoutException {
    // Create an in-memory node
    IrohNode node1 =
        IrohNode.memoryWithAlpns(java.util.List.of(ALPN.getBytes())).get(10, TimeUnit.SECONDS);

    try {
      IrohBlobs blobs = node1.blobs();

      // Check status of a non-existent blob - should return NOT_FOUND
      BlobId fakeId = BlobId.of("a".repeat(64));
      BlobStatus status = blobs.status(fakeId);
      System.out.println("Status for non-existent blob: " + status);
      assertEquals(BlobStatus.NOT_FOUND, status);

      // Check has for non-existent blob
      boolean has = blobs.has(fakeId);
      System.out.println("Has for non-existent blob: " + has);
      assertFalse(has);

    } finally {
      node1.close();
    }
  }

  @Test
  public void testAddPathImportsFile(@TempDir Path tmp) throws Exception {
    IrohNode node =
        IrohNode.memoryWithAlpns(java.util.List.of(ALPN.getBytes())).get(10, TimeUnit.SECONDS);
    try {
      IrohBlobs blobs = node.blobs();

      byte[] contents = "imported via addPath".getBytes(StandardCharsets.UTF_8);
      Path file = tmp.resolve("import-me.bin");
      Files.write(file, contents);

      BlobId id = blobs.addPathAsync(file).get(10, TimeUnit.SECONDS);
      assertNotNull(id);
      // The imported blob is present and complete in the local store.
      assertTrue(blobs.has(id));
      assertEquals(BlobStatus.COMPLETE, blobs.status(id));
    } finally {
      node.close();
    }
  }

  @Test
  public void testAddPathWithNamedTagImportsFile(@TempDir Path tmp) throws Exception {
    IrohNode node =
        IrohNode.memoryWithAlpns(java.util.List.of(ALPN.getBytes())).get(10, TimeUnit.SECONDS);
    try {
      IrohBlobs blobs = node.blobs();

      byte[] contents = "tagged via addPathWithNamedTag".getBytes(StandardCharsets.UTF_8);
      Path file = tmp.resolve("tagged.bin");
      Files.write(file, contents);

      BlobId id =
          blobs.addPathWithNamedTagAsync(file, "portal-sync/t1/blob").get(10, TimeUnit.SECONDS);
      assertNotNull(id);
      assertTrue(blobs.has(id));
      assertEquals(BlobStatus.COMPLETE, blobs.status(id));
    } finally {
      node.close();
    }
  }
}
