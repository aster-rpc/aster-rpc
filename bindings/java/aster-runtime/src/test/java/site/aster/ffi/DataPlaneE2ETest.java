package site.aster.ffi;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.blobs.BlobFormat;
import site.aster.blobs.BlobId;
import site.aster.blobs.BlobStatus;
import site.aster.blobs.FetchReport;
import site.aster.blobs.FetchResult;
import site.aster.blobs.FetchStrategy;
import site.aster.config.AsterConfig;
import site.aster.docs.AuthorId;
import site.aster.docs.AuthorityReport;
import site.aster.docs.Doc;
import site.aster.docs.Docs;
import site.aster.docs.LocalAccess;
import site.aster.docs.RetireOutcome;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.handle.IrohStream;
import site.aster.handle.PathInfo;
import site.aster.node.IrohNode;
import site.aster.node.NodeAddr;

/**
 * C ABI 3.3 data plane through the Java wrappers: ordered multi-provider fetch with a report,
 * store-only and single-provider downloads, ranged reads, tag prefix delete, GC; docs authority /
 * retire / open / import / durable set / entry_exists / content status; connection paths and
 * send-stream priority. Loopback, relays off: runs offline.
 */
final class DataPlaneE2ETest {

  private static final String ALPN = "aster-f3/1";

  static AsterConfig loopback() {
    return AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build();
  }

  static IrohNode start() throws Exception {
    return IrohNode.start(loopback(), List.of(ALPN.getBytes(StandardCharsets.UTF_8)), null)
        .get(30, TimeUnit.SECONDS);
  }

  static NodeAddr addr(IrohNode n) throws InterruptedException {
    NodeAddr a = n.nodeAddr();
    for (int i = 0; i < 100 && a.directAddresses().isEmpty(); i++) {
      Thread.sleep(50);
      a = n.nodeAddr();
    }
    assertFalse(a.directAddresses().isEmpty(), "node has a direct address");
    return a;
  }

  static <T> T await(java.util.concurrent.CompletableFuture<T> f) throws Exception {
    return f.get(30, TimeUnit.SECONDS);
  }

  @Test
  void fetchReportParsesTheCanonicalEncoding() {
    byte[] p = new byte[16 + 44 + 3];
    p[0] = 1;
    p[4] = 1;
    p[8] = 8;
    Arrays.fill(p, 16, 48, (byte) 0x0a);
    p[48] = 3; // attempts
    p[52] = 2; // failures
    p[56] = 1; // completed
    p[60] = 'x';
    p[61] = 'y';
    p[62] = 'z';
    FetchResult r = FetchResult.parse(p);
    assertArrayEquals("xyz".getBytes(StandardCharsets.UTF_8), r.data());
    FetchReport rep = r.report();
    assertEquals(8, rep.bytesTransferred());
    assertEquals(1, rep.providers().size());
    var o = rep.providers().get(0);
    assertEquals("0a".repeat(32), o.nodeId());
    assertEquals(3, o.attempts());
    assertTrue(o.failed() && o.served());
    p[0] = 3;
    assertThrows(IllegalArgumentException.class, () -> FetchReport.parse(p));
    p[0] = 2; // a v2 header over a v1-sized record is truncated
    assertThrows(IllegalArgumentException.class, () -> FetchReport.parse(p));
    assertThrows(IllegalArgumentException.class, () -> FetchReport.parse(new byte[4]));
  }

  @Test
  void fetchReportV2ParsesPerProviderCounters() {
    java.nio.ByteBuffer b =
        java.nio.ByteBuffer.allocate(16 + 72 * 2 + 2).order(java.nio.ByteOrder.LITTLE_ENDIAN);
    b.put((byte) 2).put(new byte[3]).putInt(2).putLong(1000);
    for (int i = 0; i < 2; i++) {
      byte[] id = new byte[32];
      Arrays.fill(id, (byte) (0x10 + i));
      b.put(id);
      b.putInt(2).putInt(i).putInt(1); // attempts, failures, completed
      b.putInt(i).putInt(1 - i); // in_flight, plain_fallbacks
      b.putInt(i == 0 ? 1 : 0); // flags: HAS_WIRE_BYTES on the first
      b.putLong(600 - 200L * i).putLong(i == 0 ? 450 : 0);
    }
    b.put((byte) 'o').put((byte) 'k');
    FetchResult r = FetchResult.parse(b.array());
    assertArrayEquals("ok".getBytes(StandardCharsets.UTF_8), r.data());
    FetchReport rep = r.report();
    assertEquals(1000, rep.bytesTransferred());
    var first = rep.providers().get(0);
    var second = rep.providers().get(1);
    assertEquals("10".repeat(32), first.nodeId());
    assertEquals(0, first.inFlight());
    assertEquals(1, first.plainFallbacks());
    assertEquals(600, first.bytesTransferred());
    assertEquals(450L, first.wireBytes());
    assertEquals(1, second.inFlight());
    assertEquals(400, second.bytesTransferred());
    assertEquals(null, second.wireBytes());
    assertEquals(List.of("11".repeat(32)), rep.inFlight());
    assertEquals(List.of("10".repeat(32)), rep.plainOnly());
  }

  @Test
  void blobsFetchRangeTagsAndGc() throws Exception {
    try (IrohNode provider = start();
        IrohNode fetcher = start()) {
      // A real node id that nobody serves any more: tried first, fails over.
      String dead;
      try (IrohNode gone = start()) {
        dead = gone.nodeId();
      }
      byte[] big = new byte[150_000];
      for (int i = 0; i < big.length; i++) big[i] = (byte) (i % 251);
      BlobId h = await(provider.blobs().addBytesAsync(big));
      BlobId small =
          await(provider.blobs().addBytesAsync("store".getBytes(StandardCharsets.UTF_8)));
      fetcher.addNodeAddr(addr(provider));
      String pid = provider.nodeId();

      FetchResult r =
          await(
              fetcher
                  .blobs()
                  .downloadHashFromAsync(
                      h, List.of(dead, pid), BlobFormat.RAW, FetchStrategy.WHOLE, false));
      assertArrayEquals(big, r.data());
      assertEquals(List.of(dead, pid), r.report().attempted());
      assertEquals(List.of(dead), r.report().failed());
      assertEquals(List.of(pid), r.report().served());
      assertTrue(r.report().bytesTransferred() >= big.length);
      // Fetch report v2 (ABI 7.4): per-provider payload bytes.
      var served = r.report().providers().get(1);
      assertEquals(pid, served.nodeId());
      assertTrue(served.bytesTransferred() >= big.length, served.toString());
      assertEquals(0, served.inFlight());
      assertEquals(0, r.report().providers().get(0).bytesTransferred());

      FetchResult stored =
          await(
              fetcher
                  .blobs()
                  .downloadHashFromAsync(
                      small, List.of(pid), BlobFormat.RAW, FetchStrategy.WHOLE, true));
      assertEquals(0, stored.data().length);
      assertEquals(List.of(pid), stored.report().served());

      assertArrayEquals(
          Arrays.copyOfRange(big, 500, 600), await(fetcher.blobs().readRangeAsync(h, 500, 100)));
      assertArrayEquals(
          "store".getBytes(StandardCharsets.UTF_8),
          await(fetcher.blobs().downloadHashAsync(small, pid, BlobFormat.RAW, false)));

      // SPLIT_CHILDREN on a raw blob is refused before anything starts.
      var bad =
          fetcher
              .blobs()
              .downloadHashFromAsync(
                  h, List.of(pid), BlobFormat.RAW, FetchStrategy.SPLIT_CHILDREN, false);
      ExecutionException e =
          assertThrows(ExecutionException.class, () -> bad.get(5, TimeUnit.SECONDS));
      assertTrue(e.getCause() instanceof IrohException, e.toString());

      for (String name : List.of("f3/a", "f3/b", "keep")) {
        await(fetcher.tags().setAsync(name, h, site.aster.tags.TagFormat.RAW));
      }
      assertEquals(2L, await(fetcher.tags().deletePrefixAsync("f3/")));
      await(fetcher.blobs().gcRunOnceAsync());
    }
  }

  @Test
  void docsAuthorityRetireOpenAndEntries() throws Exception {
    try (IrohNode node = start()) {
      Docs docs = node.docs();
      byte[] secret = new byte[32];
      Arrays.fill(secret, (byte) 7);
      Doc doc = await(docs.importWriteNamespaceAsync(secret));
      String ns = doc.docId();
      assertEquals(64, ns.length());

      AuthorityReport rep = await(docs.authorityReportAsync(ns));
      assertEquals(ns, rep.namespaceId());
      assertEquals(LocalAccess.WRITE, rep.access());
      assertArrayEquals(Docs.authorityFingerprint(ns, LocalAccess.WRITE), rep.fingerprint());
      assertEquals(rep, await(doc.authorityReportAsync()));

      AuthorId author = await(docs.createAuthorAsync());
      byte[] key = "k".getBytes(StandardCharsets.UTF_8);
      BlobId content =
          await(doc.setBytesDurableAsync(author, key, "v1".getBytes(StandardCharsets.UTF_8)));
      assertTrue(await(doc.entryExistsAsync(author, key)));
      assertFalse(await(doc.entryExistsAsync(author, "nope".getBytes(StandardCharsets.UTF_8))));
      Doc.ContentStatus cs = await(doc.entryContentStatusAsync(content));
      assertEquals(BlobStatus.COMPLETE, cs.status());
      assertEquals(2L, cs.size());

      RetireOutcome busy = await(docs.retireLocalWriteAuthorityAsync(ns));
      assertTrue(busy.busy());
      assertTrue(busy.openReferences() >= 1);

      await(doc.shutdownForRoleSwapAsync(Duration.ofSeconds(5)));
      RetireOutcome done = await(docs.retireLocalWriteAuthorityAsync(ns));
      assertTrue(done.retired());
      assertEquals(LocalAccess.READ, done.access());
      assertEquals(LocalAccess.READ, await(docs.authorityReportAsync(ns)).access());

      Doc reopened = await(docs.openAsync(ns)).orElseThrow();
      assertEquals(ns, reopened.docId());
      assertEquals(ns, await(docs.importReadNamespaceAsync(ns)).docId());
      assertTrue(await(docs.openAsync("cd".repeat(32))).isEmpty());
      assertThrows(
          ExecutionException.class, () -> docs.authorityReportAsync("cd".repeat(32)).get());
    }
  }

  @Test
  void connectionPathsAndStreamPriority() throws Exception {
    try (IrohNode server = start();
        IrohNode client = start()) {
      // Keep accepted connections referenced for the test's duration.
      var accepted = new java.util.concurrent.CopyOnWriteArrayList<Object>();
      server.connections().collect(accepted::add, java.util.concurrent.ForkJoinPool.commonPool());

      IrohConnection conn =
          await(
              IrohEndpoint.ofNode(client.runtime(), client.nodeHandle())
                  .connectNodeAddrAsync(addr(server), ALPN));
      List<PathInfo> paths = conn.paths();
      assertFalse(paths.isEmpty());
      PathInfo direct =
          paths.stream().filter(p -> p.kind() == PathInfo.Kind.DIRECT).findFirst().orElseThrow();
      assertEquals("127.0.0.1", direct.peerHost());
      assertTrue(direct.peerPort() > 0);
      PathInfo sel = conn.selectedPath().orElseThrow();
      assertTrue(sel.selected());

      IrohStream stream = await(conn.openBiAsync());
      assertEquals(0, await(stream.priorityAsync()));
      await(stream.setPriorityAsync(-7));
      assertEquals(-7, await(stream.priorityAsync()));
    }
  }
}
