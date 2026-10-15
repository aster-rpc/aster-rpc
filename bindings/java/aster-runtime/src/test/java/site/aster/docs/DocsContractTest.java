package site.aster.docs;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.config.AsterConfig;
import site.aster.ffi.IrohException;
import site.aster.node.IrohNode;

final class DocsContractTest {
  private static byte[] utf8(String value) {
    return value.getBytes(StandardCharsets.UTF_8);
  }

  private static <T> T await(CompletableFuture<T> future) throws Exception {
    return future.get(30, TimeUnit.SECONDS);
  }

  private static IrohNode start() throws Exception {
    return await(
        IrohNode.start(
            AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build(),
            List.of(),
            null));
  }

  @Test
  void rawValuesMetadataQueryModesAndDeletion() throws Exception {
    try (IrohNode node = start();
        Doc doc = await(node.docs().createAsync())) {
      AuthorId author = await(node.docs().createAuthorAsync());
      byte[] raw = new byte[200];
      for (int i = 0; i < raw.length; i++) raw[i] = (byte) i;
      await(doc.setBytesAsync(author, "prefix/one", raw));
      await(doc.setBytesAsync(author, "prefix/two", utf8("second")));
      assertThrows(Exception.class, () -> await(doc.setBytesAsync(author, "empty", new byte[0])));
      await(doc.setBytesAsync(author, "other", utf8("other")));
      byte[] binaryKey = {(byte) 0xff, 0, 1};
      await(doc.setBytesAsync(author, binaryKey, utf8("binary key")));

      assertArrayEquals(raw, await(doc.getExactAsync(author, "prefix/one")));
      assertArrayEquals(utf8("other"), await(doc.getExactAsync(author, "other")));
      assertNull(await(doc.getExactAsync(author, "missing")));
      assertArrayEquals(utf8("binary key"), await(doc.getExactAsync(author, binaryKey)));

      List<DocEntry> exact = await(doc.queryAsync(QueryMode.KEY_EXACT, "prefix/one"));
      assertEquals(1, exact.size());
      DocEntry entry = exact.getFirst();
      assertEquals(author, entry.author());
      assertEquals("prefix/one", entry.key());
      assertArrayEquals(utf8("prefix/one"), entry.keyBytes());
      assertEquals(raw.length, entry.contentLen());
      assertTrue(entry.timestamp() > 0);
      assertArrayEquals(raw, await(doc.readEntryContentAsync(entry.contentHash())));
      assertEquals(2, await(doc.queryAsync(QueryMode.KEY_PREFIX, "prefix/")).size());
      assertEquals(1, await(doc.queryAsync(QueryMode.KEY_PREFIX, "prefix/", 1)).size());
      assertEquals(1, await(doc.queryAsync(QueryMode.LATEST_EXACT, "prefix/one")).size());
      assertEquals(2, await(doc.queryAsync(QueryMode.LATEST_PREFIX, "prefix/")).size());
      assertArrayEquals(
          binaryKey,
          await(doc.queryAsync(QueryMode.KEY_EXACT, binaryKey, 0)).getFirst().keyBytes());
      assertEquals(4, await(doc.queryAsync(QueryMode.KEY_PREFIX, "")).size());

      assertEquals(2L, await(doc.deleteAsync(author, "prefix/")));
      assertNull(await(doc.getExactAsync(author, "prefix/one")));
      assertTrue(await(doc.queryAsync(QueryMode.LATEST_PREFIX, "prefix/")).isEmpty());
      // Prefix tombstones can themselves be replaced by later deletions.
      assertTrue(await(doc.deleteAsync(author, new byte[0])) >= 2L);
      assertThrows(Exception.class, () -> await(doc.queryAsync(QueryMode.KEY_PREFIX, "", -1)));
      assertThrows(Exception.class, () -> await(doc.startSyncAsync(List.of("invalid-peer"))));
      assertThrows(Exception.class, () -> await(doc.shareWithAddrAsync(999)));
    }
  }

  @Test
  void explicitPeersSyncAndAddressedTicketJoin() throws Exception {
    try (IrohNode source = start();
        IrohNode replica = start();
        IrohNode joined = start();
        Doc doc = await(source.docs().createAsync())) {
      AuthorId author = await(source.docs().createAuthorAsync());
      await(doc.setBytesAsync(author, "sync/value", utf8("first")));
      var addr = source.nodeAddr();
      for (int i = 0; i < 100 && addr.directAddresses().isEmpty(); i++) {
        Thread.sleep(20);
        addr = source.nodeAddr();
      }
      assertFalse(addr.directAddresses().isEmpty());
      // Sharing enables this namespace's native sync service before another
      // node initiates a peer sync.
      await(doc.shareWithAddrAsync(0));
      try (Doc imported = await(replica.docs().importReadNamespaceAsync(doc.docId()))) {
        replica.addNodeAddr(addr);
        await(imported.startSyncAsync(List.of(source.nodeId())));
        awaitValue(imported, author, "sync/value", utf8("first"));
        await(doc.setBytesAsync(author, "sync/value", utf8("updated")));
        awaitValue(imported, author, "sync/value", utf8("updated"));
        await(imported.leaveAsync());
      }
      try (Doc received = await(joined.docs().joinAsync(await(doc.shareWithAddrAsync(0))))) {
        assertEquals(doc.docId(), received.docId());
        // Join imports the capability and address hints; sync starts explicitly.
        await(received.startSyncAsync(List.of(source.nodeId())));
        awaitValue(received, author, "sync/value", utf8("updated"));
      }
    }
  }

  private static void awaitValue(Doc doc, AuthorId author, String key, byte[] want)
      throws Exception {
    long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
    while (System.nanoTime() < end) {
      try {
        if (Arrays.equals(want, await(doc.getExactAsync(author, key)))) return;
      } catch (java.util.concurrent.ExecutionException ignored) {
        // Metadata can arrive before its blob has downloaded.
      }
      Thread.sleep(20);
    }
    assertArrayEquals(want, await(doc.getExactAsync(author, key)));
  }

  private static byte[] metadata(byte[] key) {
    var out = new ByteArrayOutputStream();
    for (byte[] field : List.of(utf8("a1".repeat(32)), key, utf8("b2".repeat(32)))) {
      out.writeBytes(
          ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(field.length).array());
      out.writeBytes(field);
    }
    out.writeBytes(
        ByteBuffer.allocate(16)
            .order(ByteOrder.LITTLE_ENDIAN)
            .putLong(1L << 40)
            .putLong(123456789)
            .array());
    return out.toByteArray();
  }

  @Test
  void receiverTimeoutCancellationCloseAndIndependentOwnership() throws Exception {
    try (IrohNode node = start();
        Doc doc = await(node.docs().createAsync());
        DocEvents events = await(doc.subscribeAsync())) {
      AuthorId author = await(node.docs().createAuthorAsync());
      var timeout = events.nextAsync(Duration.ofMillis(30));
      var timedOut =
          assertThrows(java.util.concurrent.ExecutionException.class, () -> await(timeout));
      assertInstanceOf(java.util.concurrent.TimeoutException.class, timedOut.getCause());
      var canceled = events.nextAsync();
      assertThrows(java.util.concurrent.ExecutionException.class, () -> await(events.nextAsync()));
      assertTrue(canceled.cancel(false));
      assertTrue(canceled.isDone());
      assertTrue(canceled.isCancelled());
      assertThrows(java.util.concurrent.CancellationException.class, () -> await(canceled));
      var pending = events.nextAsync(Duration.ofSeconds(5));
      await(doc.setBytesAsync(author, "live/key", utf8("value")));
      DocEvent event = await(pending);
      assertEquals(DocEventType.INSERT_LOCAL, event.type());
      assertEquals("live/key", event.entry().key());
      assertEquals(5, event.entry().contentLen());
      assertTrue(event.entry().timestamp() > 0);
      // A second handle can write while the original Doc owner is gone.
      try (Doc writer = await(node.docs().openAsync(doc.docId())).orElseThrow()) {
        doc.close();
        var next = events.nextAsync(Duration.ofSeconds(5));
        await(writer.setBytesAsync(author, "after/close", utf8("still subscribed")));
        assertEquals("after/close", await(next).entry().key());
      }
      var closed = events.nextAsync();
      events.close();
      assertNull(await(closed));
      assertNull(await(events.nextAsync()));
      events.close();
    }
  }

  @Test
  void cancellationRacingCommittedEventsDoesNotLoseChanges() throws Exception {
    try (IrohNode node = start();
        Doc doc = await(node.docs().createAsync());
        DocEvents events = await(doc.subscribeAsync())) {
      AuthorId author = await(node.docs().createAuthorAsync());
      for (int i = 0; i < 30; i++) {
        String key = "race/" + i;
        var receiving = events.nextAsync();
        boolean canceled;
        if (i % 2 == 0) {
          canceled = receiving.cancel(false);
          await(doc.setBytesAsync(author, key, utf8("value")));
        } else {
          await(doc.setBytesAsync(author, key, utf8("value")));
          canceled = receiving.cancel(false);
        }
        DocEvent event;
        if (canceled) {
          assertTrue(receiving.isDone());
          assertTrue(receiving.isCancelled());
          event = await(events.nextAsync(Duration.ofSeconds(5)));
        } else event = await(receiving);
        assertEquals(key, event.entry().key());
      }
    }
  }

  @Test
  void joinAndSubscribeOwnsReceiverAndDoc() throws Exception {
    try (IrohNode source = start();
        IrohNode joined = start();
        Doc doc = await(source.docs().createAsync())) {
      var addr = source.nodeAddr();
      for (int i = 0; i < 100 && addr.directAddresses().isEmpty(); i++) {
        Thread.sleep(20);
        addr = source.nodeAddr();
      }
      try (DocSubscription subscription =
          await(joined.docs().joinAndSubscribeAsync(await(doc.shareWithAddrAsync(0))))) {
        assertEquals(doc.docId(), subscription.doc().docId());
        AuthorId author = await(source.docs().createAuthorAsync());
        await(doc.setBytesAsync(author, "joined/live", utf8("remote")));
        // A sync round may end before or after the insert arrives; wait for both.
        DocEvent remote = null;
        DocEvent sync = null;
        for (int i = 0; i < 40 && (remote == null || sync == null); i++) {
          DocEvent event = await(subscription.events().nextAsync(Duration.ofSeconds(5)));
          assertNotNull(event);
          if (event.type() == DocEventType.INSERT_REMOTE) remote = event;
          if (event.type() == DocEventType.SYNC_FINISHED && event.sync().succeeded()) sync = event;
        }
        assertNotNull(remote);
        assertEquals("joined/live", remote.entry().key());
        assertEquals(source.nodeId(), remote.peer());
        assertNotNull(sync);
        assertEquals(source.nodeId(), sync.peer());
        assertNull(sync.sync().error());
      }
    }
  }

  @Test
  void eventDecoderPreservesMetadataAndRejectsUnknownOrMalformedEvents() {
    byte[] metadata = metadata(utf8("event/key"));
    assertEquals("event/key", DocEvent.decode(0, metadata).entry().key());
    var out = new ByteArrayOutputStream();
    out.writeBytes(metadata);
    out.writeBytes(ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(64).array());
    out.writeBytes(utf8("c3".repeat(32)));
    assertEquals("c3".repeat(32), DocEvent.decode(1, out.toByteArray()).peer());
    assertEquals("d4".repeat(32), DocEvent.decode(2, utf8("d4".repeat(32))).contentHash().hex());
    assertEquals(DocEventType.PENDING_CONTENT_READY, DocEvent.decode(3, new byte[0]).type());
    for (int subtype : List.of(4, 5))
      assertEquals("e5".repeat(32), DocEvent.decode(subtype, utf8("e5".repeat(32))).peer());
    // SyncFinished: length-prefixed peer, origin, ok flag, length-prefixed error.
    DocEvent completed = DocEvent.decode(6, syncFinished(1, 1, ""));
    assertEquals("e5".repeat(32), completed.peer());
    assertEquals(new DocSyncResult(DocSyncOrigin.CONNECT, true, null), completed.sync());
    assertEquals(
        new DocSyncResult(DocSyncOrigin.ACCEPT, false, "no"),
        DocEvent.decode(6, syncFinished(0, 0, "no")).sync());
    assertThrows(IrohException.class, () -> DocEvent.decode(6, utf8("e5".repeat(32))));
    assertThrows(IrohException.class, () -> DocEvent.decode(6, syncFinished(2, 1, "")));
    assertThrows(IrohException.class, () -> DocEvent.decode(6, syncFinished(1, 2, "")));
    assertThrows(IrohException.class, () -> DocEvent.decode(6, syncFinished(1, 1, "x")));
    byte[] full = syncFinished(1, 1, "");
    assertThrows(
        IrohException.class, () -> DocEvent.decode(6, Arrays.copyOf(full, full.length - 1)));
    assertThrows(IrohException.class, () -> DocEvent.decode(99, new byte[0]));
    assertThrows(
        IrohException.class,
        () -> DocEvent.decode(0, Arrays.copyOf(metadata, metadata.length - 1)));
    assertThrows(IrohException.class, () -> DocEvent.decode(1, metadata));
    assertThrows(IrohException.class, () -> DocEvent.decode(2, utf8("bad hash")));
    assertThrows(IrohException.class, () -> DocEvent.decode(3, new byte[] {1}));
  }

  private static byte[] syncFinished(int origin, int ok, String error) {
    var out = new ByteArrayOutputStream();
    out.writeBytes(ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(64).array());
    out.writeBytes(utf8("e5".repeat(32)));
    out.write(origin);
    out.write(ok);
    byte[] err = utf8(error);
    out.writeBytes(
        ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(err.length).array());
    out.writeBytes(err);
    return out.toByteArray();
  }

  @Test
  void metadataDecoderRejectsMalformedAndTruncatedPayloads() {
    byte[] key = {0, (byte) 0xff};
    byte[] data = metadata(key);
    DocEntry entry = DocEntry.decodeList(data, 1).getFirst();
    assertArrayEquals(key, entry.keyBytes());
    assertEquals(1L << 40, entry.contentLen());
    assertEquals(123456789, entry.timestamp());
    for (int n = 0; n < data.length; n++) {
      byte[] truncated = Arrays.copyOf(data, n);
      assertThrows(IrohException.class, () -> DocEntry.decodeList(truncated, 1), "length " + n);
    }
    assertThrows(IrohException.class, () -> DocEntry.decodeList(data, 0));
    assertThrows(IrohException.class, () -> DocEntry.decodeList(data, 2));
    assertThrows(
        IrohException.class, () -> DocEntry.decodeList(Arrays.copyOf(data, data.length + 1), 1));
    byte[] badLen = data.clone();
    Arrays.fill(badLen, 0, 4, (byte) 0xff);
    assertThrows(IrohException.class, () -> DocEntry.decodeList(badLen, 1));
    byte[] badHex = data.clone();
    badHex[4] = 'z';
    assertThrows(IrohException.class, () -> DocEntry.decodeList(badHex, 1));
    assertTrue(DocEntry.decodeList(new byte[0], 0).isEmpty());
  }
}
