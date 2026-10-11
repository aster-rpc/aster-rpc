package site.aster.docs;

import static org.junit.jupiter.api.Assertions.*;

import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.blobs.BlobFormat;
import site.aster.blobs.BlobId;
import site.aster.config.AsterConfig;
import site.aster.node.IrohNode;

public final class DocContentsBatchE2ETest {
  public static IrohNode node() throws Exception {
    return await(
        IrohNode.start(
            AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build()));
  }

  static <T> T await(CompletableFuture<T> future) throws Exception {
    return future.get(30, TimeUnit.SECONDS);
  }

  @Test
  void preservesOrderDuplicatesEmptyMissingAndLargeValuesWithoutRetention() throws Exception {
    var node = node();
    try (var runtime = node.runtime();
        node;
        var doc = await(node.docs().createAsync())) {
      byte[] large = new byte[128 * 1024];
      for (int i = 0; i < large.length; i++) large[i] = (byte) i;
      BlobId first;
      BlobId empty;
      BlobId big;
      try (var custody = node.blobs().custody()) {
        first = await(custody.addBytesAsync(new byte[] {42}, BlobFormat.RAW));
        empty = await(custody.addBytesAsync(new byte[0], BlobFormat.RAW));
        big = await(custody.addBytesAsync(large, BlobFormat.RAW));
        var result =
            await(
                doc.readEntryContentsIfCompleteAsync(
                    List.of(big, first, empty, BlobId.of("00".repeat(32)), first)));
        assertEquals(5, result.size());
        assertArrayEquals(large, result.get(0).orElseThrow());
        assertArrayEquals(new byte[] {42}, result.get(1).orElseThrow());
        assertArrayEquals(new byte[0], result.get(2).orElseThrow());
        assertTrue(result.get(3).isEmpty());
        result.get(1).orElseThrow()[0] = 9;
        assertArrayEquals(new byte[] {42}, result.get(4).orElseThrow());
        assertTrue(await(doc.readEntryContentsIfCompleteAsync(List.of())).isEmpty());
        assertTrue(await(node.tags().listAllAsync()).isEmpty());
      }
      await(node.blobs().gcRunOnceAsync());
      assertTrue(
          await(doc.readEntryContentsIfCompleteAsync(List.of(big, first))).stream()
              .allMatch(java.util.Optional::isEmpty));
    }
  }

  @Test
  void canceledWaiterDoesNotBreakRetryOrRetainContent() throws Exception {
    var node = node();
    try (var runtime = node.runtime();
        node;
        var doc = await(node.docs().createAsync())) {
      BlobId hash;
      try (var custody = node.blobs().custody()) {
        hash = await(custody.addBytesAsync(new byte[128 * 1024], BlobFormat.RAW));
        var pending = doc.readEntryContentsIfCompleteAsync(java.util.Collections.nCopies(32, hash));
        if (pending.cancel(false)) {
          assertThrows(java.util.concurrent.CancellationException.class, pending::join);
        } else assertEquals(32, await(pending).size());
        assertEquals(
            128 * 1024,
            await(doc.readEntryContentsIfCompleteAsync(List.of(hash)))
                .getFirst()
                .orElseThrow()
                .length);
      }
      await(node.blobs().gcRunOnceAsync());
      assertTrue(await(doc.readEntryContentsIfCompleteAsync(List.of(hash))).getFirst().isEmpty());
      assertTrue(await(node.tags().listAllAsync()).isEmpty());
    }
  }

  @Test
  void readsOnlyLocalStoreAndDoesNotFetchFromReachableProvider() throws Exception {
    var source = node();
    var receiver = node();
    try (var sourceRuntime = source.runtime();
        var receiverRuntime = receiver.runtime();
        source;
        receiver;
        var sourceDoc = await(source.docs().createAsync());
        var targetDoc = await(receiver.docs().createAsync());
        var custody = source.blobs().custody()) {
      var hash = await(custody.addBytesAsync(new byte[] {99}, BlobFormat.RAW));
      var address = source.nodeAddr();
      for (int i = 0; i < 100 && address.directAddresses().isEmpty(); i++) {
        Thread.sleep(10);
        address = source.nodeAddr();
      }
      receiver.addNodeAddr(address);
      assertArrayEquals(
          new byte[] {99},
          await(sourceDoc.readEntryContentsIfCompleteAsync(List.of(hash)))
              .getFirst()
              .orElseThrow());
      assertTrue(
          await(targetDoc.readEntryContentsIfCompleteAsync(List.of(hash, hash))).stream()
              .allMatch(java.util.Optional::isEmpty));
      assertEquals(site.aster.blobs.BlobStatus.NOT_FOUND, receiver.blobs().status(hash));
      assertTrue(await(receiver.tags().listAllAsync()).isEmpty());
    }
  }
}
