package com.aster.blobs;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

/** Unit tests for blob types and FFI bindings. */
public class IrohBlobsTest {

  @Test
  public void blobId_validHex_accepts() {
    String hex64 = "a".repeat(64);
    BlobId id = BlobId.of(hex64);
    assertEquals(hex64, id.hex());
  }

  @Test
  public void blobId_invalidLength_rejects() {
    assertThrows(IllegalArgumentException.class, () -> BlobId.of("too-short"));
    assertThrows(IllegalArgumentException.class, () -> BlobId.of("a".repeat(63)));
    assertThrows(IllegalArgumentException.class, () -> BlobId.of("a".repeat(65)));
  }

  @Test
  public void blobId_ofBytes_createsFrom32Bytes() {
    byte[] bytes = new byte[32];
    BlobId id = BlobId.of(bytes);
    assertNotNull(id.hex());
    assertEquals(64, id.hex().length());
  }

  @Test
  public void blobId_ofBytes_wrongLength_rejects() {
    byte[] bytes = new byte[31];
    assertThrows(IllegalArgumentException.class, () -> BlobId.of(bytes));
  }

  @Test
  public void blobStatus_fromCode() {
    assertEquals(BlobStatus.NOT_FOUND, BlobStatus.fromCode(0));
    assertEquals(BlobStatus.PARTIAL, BlobStatus.fromCode(1));
    assertEquals(BlobStatus.COMPLETE, BlobStatus.fromCode(2));
    assertEquals(BlobStatus.NOT_FOUND, BlobStatus.fromCode(999)); // unknown → NOT_FOUND
  }

  @Test
  public void blobFormat_codes() {
    assertEquals(0, BlobFormat.RAW.code);
    assertEquals(1, BlobFormat.HASH_SEQ.code);
  }

  @Test
  public void blobTicket_validString() {
    BlobTicket ticket = BlobTicket.of("blob1abc123");
    assertEquals("blob1abc123", ticket.ticket());
  }

  @Test
  public void blobTicket_nullOrEmpty_rejects() {
    assertThrows(IllegalArgumentException.class, () -> BlobTicket.of(null));
    assertThrows(IllegalArgumentException.class, () -> BlobTicket.of(""));
  }

  @Test
  public void blobEntry_constructor() {
    BlobId hash = BlobId.of("a".repeat(64));
    BlobEntry entry = new BlobEntry("myfile.txt", hash, 1024);
    assertEquals("myfile.txt", entry.name());
    assertEquals(hash, entry.hash());
    assertEquals(1024, entry.size());
  }

  @Test
  public void blobCollection_empty() {
    BlobCollection collection = new BlobCollection(java.util.Collections.emptyList());
    assertTrue(collection.entries().isEmpty());
  }

  @Test
  public void blobCollection_withEntries() {
    BlobId hash = BlobId.of("b".repeat(64));
    BlobEntry entry = new BlobEntry("test.bin", hash, 2048);
    BlobCollection collection = new BlobCollection(java.util.List.of(entry));
    assertEquals(1, collection.entries().size());
    assertEquals("test.bin", collection.entries().get(0).name());
  }

  @Test
  public void blobInfo_constructor() {
    BlobId hash = BlobId.of("c".repeat(64));
    BlobInfo info = new BlobInfo(hash, 4096, BlobStatus.COMPLETE);
    assertEquals(hash, info.hash());
    assertEquals(4096, info.size());
    assertEquals(BlobStatus.COMPLETE, info.status());
  }

  @Test
  public void blobStatus_toString() {
    assertEquals("NOT_FOUND", BlobStatus.NOT_FOUND.toString());
    assertEquals("PARTIAL", BlobStatus.PARTIAL.toString());
    assertEquals("COMPLETE", BlobStatus.COMPLETE.toString());
  }
}
