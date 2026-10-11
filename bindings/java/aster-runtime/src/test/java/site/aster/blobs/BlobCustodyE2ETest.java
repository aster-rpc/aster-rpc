package site.aster.blobs;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HexFormat;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import site.aster.config.AsterConfig;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;
import site.aster.node.IrohNode;
import site.aster.tags.TagFormat;

/** Actual ABI 4.13 imports, temporary GC custody and named root handover. */
public class BlobCustodyE2ETest {
  static <T> T await(CompletableFuture<T> operation) throws Exception {
    return operation.get(30, TimeUnit.SECONDS);
  }

  public static IrohNode node() throws Exception {
    return await(
        IrohNode.start(
            AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build()));
  }

  @Test
  void rawCustodySealsAdmissionAndRetainsAcceptedWork() throws Exception {
    var node = node();
    try (var runtime = node.runtime();
        node) {
      var blobs = node.blobs();
      var scope = blobs.custody();
      var hash =
          await(scope.addBytesAsync("temporary".getBytes(StandardCharsets.UTF_8), BlobFormat.RAW));
      assertTrue(await(node.tags().listAllAsync()).isEmpty());
      await(blobs.gcRunOnceAsync());
      assertEquals(BlobStatus.COMPLETE, blobs.status(hash));
      var reading = scope.readAsync(hash);
      scope.close();
      assertArrayEquals("temporary".getBytes(StandardCharsets.UTF_8), await(reading));
      assertThrows(IllegalStateException.class, () -> scope.readAsync(hash));
      scope.close();
      await(blobs.gcRunOnceAsync());
      assertEquals(BlobStatus.NOT_FOUND, blobs.status(hash));
    }
  }

  @Test
  void hashSeqIsNotCollectionAndRetainsChildren() throws Exception {
    var node = node();
    try (var runtime = node.runtime();
        node) {
      var blobs = node.blobs();
      BlobId child;
      BlobId root;
      byte[] sequence;
      try (var scope = blobs.custody()) {
        child = await(scope.addBytesAsync(new byte[] {42}, BlobFormat.RAW));
        sequence = HexFormat.of().parseHex(child.hex());
        root = await(scope.addBytesAsync(sequence, BlobFormat.HASH_SEQ));
        assertArrayEquals(sequence, await(scope.readAsync(root)));
        await(scope.retainAsync(root, BlobFormat.HASH_SEQ, "custody/树"));
      }
      await(blobs.gcRunOnceAsync());
      assertEquals(BlobStatus.COMPLETE, blobs.status(root));
      assertEquals(BlobStatus.COMPLETE, blobs.status(child));
      var tag = await(node.tags().getAsync("custody/树"));
      assertEquals(root, tag.hash());
      assertEquals(TagFormat.HASH_SEQ, tag.format());
      await(node.tags().deleteAsync("custody/树"));
      await(blobs.gcRunOnceAsync());
      assertEquals(BlobStatus.NOT_FOUND, blobs.status(root));
      assertEquals(BlobStatus.NOT_FOUND, blobs.status(child));
    }
  }

  @Test
  void verifiedCopyRejectsMismatchWithoutReplacingNamedRoot(@TempDir Path temp) throws Exception {
    var node = node();
    try (var runtime = node.runtime();
        node) {
      var blobs = node.blobs();
      byte[] first = "first".getBytes(StandardCharsets.UTF_8);
      byte[] second = "second".getBytes(StandardCharsets.UTF_8);
      BlobId h1;
      BlobId h2;
      try (var scope = blobs.custody()) {
        h1 = await(scope.addBytesAsync(first, BlobFormat.RAW));
        h2 = await(scope.addBytesAsync(second, BlobFormat.RAW));
      }
      await(blobs.addBytesVerifiedAsync(first, h1, BlobFormat.RAW, "custody/current"));
      var failure =
          assertThrows(
              java.util.concurrent.ExecutionException.class,
              () ->
                  await(
                      blobs.addBytesVerifiedAsync(second, h1, BlobFormat.RAW, "custody/current")));
      assertEquals(IrohStatus.VERIFICATION_FAILED, ((IrohException) failure.getCause()).status());
      assertEquals(h1, await(node.tags().getAsync("custody/current")).hash());
      Path file = temp.resolve("copy");
      Files.write(file, second);
      await(blobs.addPathVerifiedAsync(file, h2, BlobFormat.RAW, "custody/current"));
      Files.writeString(file, "mutated");
      await(blobs.gcRunOnceAsync());
      assertEquals(BlobStatus.NOT_FOUND, blobs.status(h1));
      assertArrayEquals(second, await(blobs.readAsync(h2.hex())));
      try (var reading = blobs.custody()) {
        await(reading.protectAsync(h2, BlobFormat.RAW));
        assertArrayEquals(second, await(reading.readAsync(h2)));
      }
      assertEquals(1, await(node.tags().listAllAsync()).size());
      await(node.tags().deleteAsync("custody/current"));
      await(blobs.gcRunOnceAsync());
      assertEquals(BlobStatus.NOT_FOUND, blobs.status(h2));
    }
  }

  @Test
  void unprotectedHandoverAndWrongFormatAreRejected() throws Exception {
    var node = node();
    try (var runtime = node.runtime();
        node;
        var scope = node.blobs().custody()) {
      var hash = await(scope.addBytesAsync(new byte[] {1}, BlobFormat.RAW));
      for (var operation :
          List.of(
              scope.retainAsync(hash, BlobFormat.HASH_SEQ, "wrong-format"),
              scope.retainAsync(BlobId.of("00".repeat(32)), BlobFormat.RAW, "unprotected"))) {
        assertThrows(java.util.concurrent.ExecutionException.class, () -> await(operation));
      }
      assertTrue(await(node.tags().listAllAsync()).isEmpty());
    }
  }

  @Test
  void protectsAbsentHashSeqBeforePeerFetch() throws Exception {
    var provider = node();
    var fetcher = node();
    try (var providerRuntime = provider.runtime();
        var fetcherRuntime = fetcher.runtime();
        provider;
        fetcher;
        var source = provider.blobs().custody();
        var target = fetcher.blobs().custody()) {
      var child = await(source.addBytesAsync(new byte[] {99}, BlobFormat.RAW));
      var sequence = HexFormat.of().parseHex(child.hex());
      var root = await(source.addBytesAsync(sequence, BlobFormat.HASH_SEQ));
      var address = provider.nodeAddr();
      for (int i = 0; i < 100 && address.directAddresses().isEmpty(); i++) {
        Thread.sleep(10);
        address = provider.nodeAddr();
      }
      assertFalse(address.directAddresses().isEmpty());
      fetcher.addNodeAddr(address);
      await(target.protectAsync(root, BlobFormat.HASH_SEQ));
      var result =
          await(
              target.downloadHashFromAsync(
                  root,
                  List.of(provider.nodeId()),
                  BlobFormat.HASH_SEQ,
                  FetchStrategy.SPLIT_CHILDREN,
                  true));
      assertArrayEquals(new byte[0], result.data());
      await(fetcher.blobs().gcRunOnceAsync());
      assertArrayEquals(sequence, await(target.readAsync(root)));
      assertArrayEquals(new byte[] {99}, await(target.readAsync(child)));
      assertTrue(await(fetcher.tags().listAllAsync()).isEmpty());
    }
  }

  @Test
  void cancellationKeepsTerminalRegistration() throws Exception {
    var node = node();
    try (var runtime = node.runtime();
        node) {
      var blobs = node.blobs();
      byte[] data = new byte[8 * 1024 * 1024];
      BlobId expected;
      try (var scope = blobs.custody()) {
        expected = await(scope.addBytesAsync(data, BlobFormat.RAW));
      }
      var operation = blobs.addBytesVerifiedAsync(data, expected, BlobFormat.RAW, "custody/cancel");
      assertFalse(operation.cancel(false));
      operation.requestCancel();
      try {
        assertEquals(expected, await(operation)); // The irrevocable commit may already have won.
        assertEquals(expected, await(node.tags().getAsync("custody/cancel")).hash());
        await(node.tags().deleteAsync("custody/cancel"));
      } catch (java.util.concurrent.CancellationException canceled) {
        assertTrue(await(node.tags().listAllAsync()).isEmpty());
      }
      assertFalse(operation.requestCancel());
      await(blobs.gcRunOnceAsync());
      assertEquals(BlobStatus.NOT_FOUND, blobs.status(expected));
    }
  }
}
