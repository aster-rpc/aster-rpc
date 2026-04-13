package site.aster;

import static org.junit.jupiter.api.Assertions.*;

import java.time.Duration;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import org.junit.jupiter.api.Test;
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
}
