package com.aster.docs;

import com.aster.blobs.BlobId;
import com.aster.ffi.IrohEventKind;
import com.aster.ffi.IrohException;
import com.aster.ffi.IrohLibrary;
import com.aster.ffi.IrohStatus;
import com.aster.handle.IrohRuntime;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;

/**
 * A document handle for content-addressed operations.
 *
 * <p>Documents support setting/getting content, querying entries, and sync. Obtain via {@link
 * Docs#createAsync} or {@link Docs#joinAsync}.
 */
public class Doc {

  private final IrohRuntime runtime;
  private final long docHandle;

  public Doc(IrohRuntime runtime, long docHandle) {
    this.runtime = runtime;
    this.docHandle = docHandle;
  }

  private IrohRuntime runtime() {
    return runtime;
  }

  private long docHandle() {
    return docHandle;
  }

  private MemorySegment toStringSegment(String str, SegmentAllocator alloc) {
    byte[] bytes = str.getBytes(StandardCharsets.UTF_8);
    MemorySegment seg = alloc.allocate(bytes.length);
    seg.copyFrom(MemorySegment.ofArray(bytes));
    return seg;
  }

  private MemorySegment toBytesSegment(byte[] data, SegmentAllocator alloc) {
    MemorySegment dataSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    MemorySegment heapSeg = alloc.allocate(data.length);
    heapSeg.copyFrom(MemorySegment.ofArray(data));
    dataSeg.set(ValueLayout.ADDRESS, 0, heapSeg);
    dataSeg.set(ValueLayout.JAVA_LONG, 8, (long) data.length);
    return dataSeg;
  }

  /** Close this document and free its handle. */
  public void close() {
    IrohLibrary lib = IrohLibrary.getInstance();
    lib.docFree(runtime.nativeHandle(), docHandle);
  }

  /**
   * Set bytes in the document.
   *
   * @param author the author ID
   * @param key the content key
   * @param value the content value
   * @return a future that completes when the content is set
   */
  public CompletableFuture<Void> setBytesAsync(AuthorId author, String key, byte[] value) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] authorBytes = author.hex().getBytes(StandardCharsets.UTF_8);
    byte[] keyBytes = key.getBytes(StandardCharsets.UTF_8);

    var authorSeg = alloc.allocate(authorBytes.length);
    authorSeg.copyFrom(MemorySegment.ofArray(authorBytes));
    var authorBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    authorBytesSeg.set(ValueLayout.ADDRESS, 0, authorSeg);
    authorBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) authorBytes.length);

    var keySeg = alloc.allocate(keyBytes.length);
    keySeg.copyFrom(MemorySegment.ofArray(keyBytes));
    var keyBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    keyBytesSeg.set(ValueLayout.ADDRESS, 0, keySeg);
    keyBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) keyBytes.length);

    var valueBytesSeg = toBytesSegment(value, alloc);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.docSetBytes(
              runtime.nativeHandle(),
              docHandle,
              authorBytesSeg,
              keyBytesSeg,
              valueBytesSeg,
              0L,
              opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_doc_set_bytes failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_doc_set_bytes threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime.registry().register(opId).thenApply(event -> null);
  }

  /**
   * Get exact entry from the document.
   *
   * @param author the author ID
   * @param key the content key
   * @return a future that completes with the entry, or null if not found
   */
  public CompletableFuture<DocEntry> getExactAsync(AuthorId author, String key) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] authorBytes = author.hex().getBytes(StandardCharsets.UTF_8);
    byte[] keyBytes = key.getBytes(StandardCharsets.UTF_8);

    var authorSeg = alloc.allocate(authorBytes.length);
    authorSeg.copyFrom(MemorySegment.ofArray(authorBytes));
    var authorBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    authorBytesSeg.set(ValueLayout.ADDRESS, 0, authorSeg);
    authorBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) authorBytes.length);

    var keySeg = alloc.allocate(keyBytes.length);
    keySeg.copyFrom(MemorySegment.ofArray(keyBytes));
    var keyBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    keyBytesSeg.set(ValueLayout.ADDRESS, 0, keySeg);
    keyBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) keyBytes.length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.docGetExact(
              runtime.nativeHandle(), docHandle, authorBytesSeg, keyBytesSeg, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_doc_get_exact failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_doc_get_exact threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.DOC_GET) {
                if (event.status() == IrohStatus.NOT_FOUND.code) {
                  return null;
                }
                return parseDocEntry(event.data().asByteBuffer().array());
              }
              throw new IrohException("getExact failed: unexpected event " + event.kind());
            });
  }

  /**
   * Query entries from the document.
   *
   * @param mode the query mode
   * @param keyPrefix key prefix to filter (empty for all entries)
   * @return a future that completes with matching entries
   */
  public CompletableFuture<List<DocEntry>> queryAsync(QueryMode mode, String keyPrefix) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] keyBytes = keyPrefix.getBytes(StandardCharsets.UTF_8);
    var keySeg = alloc.allocate(keyBytes.length);
    keySeg.copyFrom(MemorySegment.ofArray(keyBytes));
    var keyBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    keyBytesSeg.set(ValueLayout.ADDRESS, 0, keySeg);
    keyBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) keyBytes.length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.docQuery(runtime.nativeHandle(), docHandle, mode.code(), keyBytesSeg, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_doc_query failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_doc_query threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.DOC_QUERY) {
                return parseDocEntryList(event.data().asByteBuffer().array());
              }
              throw new IrohException("query failed: unexpected event " + event.kind());
            });
  }

  /**
   * Read entry content from the document.
   *
   * @param contentHash the content hash
   * @return a future that completes with the content bytes
   */
  public CompletableFuture<byte[]> readEntryContentAsync(BlobId contentHash) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] hashBytes = contentHash.hex().getBytes(StandardCharsets.UTF_8);
    var hashSeg = alloc.allocate(hashBytes.length);
    hashSeg.copyFrom(MemorySegment.ofArray(hashBytes));
    var hashBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    hashBytesSeg.set(ValueLayout.ADDRESS, 0, hashSeg);
    hashBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) hashBytes.length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.docReadEntryContent(runtime.nativeHandle(), docHandle, hashBytesSeg, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_doc_read_entry_content failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_doc_read_entry_content threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.DOC_GET) {
                byte[] data = event.data().asByteBuffer().array();
                if (event.hasBuffer()) {
                  runtime.releaseBuffer(event.buffer());
                }
                return data;
              }
              throw new IrohException("readEntryContent failed: unexpected event " + event.kind());
            });
  }

  /**
   * Share this document.
   *
   * @param mode the share mode (0=read, 1=write)
   * @return a future that completes with the ticket
   */
  public CompletableFuture<String> shareAsync(int mode) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.docShare(runtime.nativeHandle(), docHandle, mode, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_doc_share failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_doc_share threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.DOC_SHARED) {
                byte[] data = event.data().asByteBuffer().array();
                return new String(data, StandardCharsets.UTF_8).trim();
              }
              throw new IrohException("share failed: unexpected event " + event.kind());
            });
  }

  /**
   * Start syncing this document.
   *
   * @return a future that completes when sync starts
   */
  public CompletableFuture<Void> startSyncAsync() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    // Empty peers list
    var peersSeg = alloc.allocate(IrohLibrary.IROH_BYTES_LIST);
    peersSeg.set(ValueLayout.ADDRESS, 0, MemorySegment.NULL);
    peersSeg.set(ValueLayout.JAVA_LONG, 8, 0L);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.docStartSync(runtime.nativeHandle(), docHandle, peersSeg, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_doc_start_sync failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_doc_start_sync threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime.registry().register(opId).thenApply(event -> null);
  }

  /**
   * Leave (stop syncing) this document.
   *
   * @return a future that completes when sync stops
   */
  public CompletableFuture<Void> leaveAsync() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.docLeave(runtime.nativeHandle(), docHandle, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_doc_leave failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_doc_leave threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime.registry().register(opId).thenApply(event -> null);
  }

  // Entry format: author_hex (64 chars) + key_len (4 bytes) + key + content_hash_hex (64 chars) +
  // content_len (8 bytes)
  private DocEntry parseDocEntry(byte[] data) {
    try {
      int offset = 0;

      // Read author (64 hex chars)
      String author = new String(data, offset, 64, StandardCharsets.UTF_8).trim();
      offset += 64;

      // Read key length
      long keyLen =
          java.nio.ByteBuffer.wrap(data, offset, 4)
              .order(java.nio.ByteOrder.LITTLE_ENDIAN)
              .getInt();
      offset += 4;

      // Read key
      String key = new String(data, offset, (int) keyLen, StandardCharsets.UTF_8);
      offset += keyLen;

      // Read content hash (64 hex chars)
      String contentHash = new String(data, offset, 64, StandardCharsets.UTF_8).trim();
      offset += 64;

      // Read content length
      long contentLen =
          java.nio.ByteBuffer.wrap(data, offset, 8)
              .order(java.nio.ByteOrder.LITTLE_ENDIAN)
              .getLong();
      offset += 8;

      // Read value (content_len bytes)
      byte[] value = new byte[(int) contentLen];
      System.arraycopy(data, offset, value, 0, (int) contentLen);

      return new DocEntry(key, AuthorId.of(author), BlobId.of(contentHash), value);
    } catch (Exception e) {
      throw new IrohException("Failed to parse doc entry: " + e.getMessage());
    }
  }

  private List<DocEntry> parseDocEntryList(byte[] data) {
    List<DocEntry> entries = new ArrayList<>();
    if (data == null || data.length == 0) {
      return entries;
    }

    try {
      int offset = 0;
      while (offset < data.length) {
        // Check if we have enough data for the header
        if (offset + 68 > data.length) break; // 64 (author) + 4 (key_len)

        // Read author
        String author = new String(data, offset, 64, StandardCharsets.UTF_8).trim();
        offset += 64;

        // Read key length
        long keyLen =
            java.nio.ByteBuffer.wrap(data, offset, 4)
                .order(java.nio.ByteOrder.LITTLE_ENDIAN)
                .getInt();
        offset += 4;

        if (offset + keyLen + 72 > data.length) break; // key + 64 (hash) + 8 (content_len)

        // Read key
        String key = new String(data, offset, (int) keyLen, StandardCharsets.UTF_8);
        offset += keyLen;

        // Read content hash
        String contentHash = new String(data, offset, 64, StandardCharsets.UTF_8).trim();
        offset += 64;

        // Read content length
        long contentLen =
            java.nio.ByteBuffer.wrap(data, offset, 8)
                .order(java.nio.ByteOrder.LITTLE_ENDIAN)
                .getLong();
        offset += 8;

        if (offset + contentLen > data.length) break;

        // Read value
        byte[] value = new byte[(int) contentLen];
        System.arraycopy(data, offset, value, 0, (int) contentLen);
        offset += contentLen;

        entries.add(new DocEntry(key, AuthorId.of(author), BlobId.of(contentHash), value));
      }
    } catch (Exception e) {
      // Return what we have on parse error
    }

    return entries;
  }
}
