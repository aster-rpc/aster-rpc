package site.aster.docs;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import site.aster.blobs.BlobId;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;

/**
 * A document handle. Queries return metadata; read values with getExactAsync or
 * readEntryContentAsync.
 */
public class Doc implements AutoCloseable {
  private final IrohRuntime runtime;
  private final long docHandle;
  private final long nodeHandle;

  public Doc(IrohRuntime runtime, long docHandle) {
    this(runtime, docHandle, 0L);
  }

  public Doc(IrohRuntime runtime, long docHandle, long nodeHandle) {
    this.runtime = runtime;
    this.docHandle = docHandle;
    this.nodeHandle = nodeHandle;
  }

  public IrohRuntime runtime() {
    return runtime;
  }

  public long docHandle() {
    return docHandle;
  }

  /** Subscribe to this document independently of the document handle. Close the returned owner. */
  public CompletableFuture<DocEvents> subscribeAsync() {
    return DocEvents.subscribe(this);
  }

  @Override
  public void close() {
    IrohLibrary.getInstance().docFree(runtime.nativeHandle(), docHandle);
  }

  /** The document's 64-character hex namespace identifier. */
  public String docId() {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(64);
      MemorySegment len = arena.allocate(ValueLayout.JAVA_LONG);
      len.set(ValueLayout.JAVA_LONG, 0, 64L);
      int status = IrohLibrary.getInstance().docId(runtime.nativeHandle(), docHandle, out, len);
      if (status != 0)
        throw new IrohException(IrohStatus.fromCode(status), "aster_doc_id failed: " + status);
      return new String(
          out.asSlice(0, len.get(ValueLayout.JAVA_LONG, 0)).toArray(ValueLayout.JAVA_BYTE),
          StandardCharsets.UTF_8);
    }
  }

  private static DataPlane.Result expect(DataPlane.Result r, IrohEventKind kind) {
    if (r.event().kind() != kind)
      throw new IrohException("expected " + kind + ", got " + r.event().kind());
    if (r.event().status() != IrohStatus.OK.code)
      throw new IrohException(IrohStatus.fromCode(r.event().status()), "document operation failed");
    return r;
  }

  /** Set a value under a UTF-8 key. */
  public CompletableFuture<Void> setBytesAsync(AuthorId author, String key, byte[] value) {
    return setBytesAsync(author, key.getBytes(StandardCharsets.UTF_8), value);
  }

  /** Set a value under an arbitrary byte key. */
  public CompletableFuture<Void> setBytesAsync(AuthorId author, byte[] key, byte[] value) {
    return DataPlane.docSetBytes(runtime, docHandle, author.hex(), key, value)
        .thenApply(
            r -> {
              expect(r, IrohEventKind.DOC_SET);
              return null;
            });
  }

  /** Read raw value bytes under a UTF-8 key, or null when absent. */
  public CompletableFuture<byte[]> getExactAsync(AuthorId author, String key) {
    return getExactAsync(author, key.getBytes(StandardCharsets.UTF_8));
  }

  /** Read raw value bytes under an arbitrary byte key, or null when absent. */
  public CompletableFuture<byte[]> getExactAsync(AuthorId author, byte[] key) {
    return DataPlane.docGetExact(runtime, docHandle, author.hex(), key)
        .thenApply(
            r -> {
              if (r.event().kind() == IrohEventKind.DOC_GET
                  && r.event().status() == IrohStatus.NOT_FOUND.code) return null;
              return expect(r, IrohEventKind.DOC_GET).data();
            });
  }

  /** Query metadata for a UTF-8 key/prefix without limiting the result. */
  public CompletableFuture<List<DocEntry>> queryAsync(QueryMode mode, String key) {
    return queryAsync(mode, key, 0);
  }

  /** Query metadata for a UTF-8 key/prefix; zero limit means unbounded. */
  public CompletableFuture<List<DocEntry>> queryAsync(QueryMode mode, String key, long limit) {
    return queryAsync(mode, key.getBytes(StandardCharsets.UTF_8), limit);
  }

  /** Query metadata for an arbitrary byte key/prefix; values are read separately. */
  public CompletableFuture<List<DocEntry>> queryAsync(QueryMode mode, byte[] key, long limit) {
    if (limit < 0)
      return CompletableFuture.failedFuture(
          new IllegalArgumentException("limit must be nonnegative"));
    return DataPlane.docQuery(runtime, docHandle, mode.code(), key, limit)
        .thenApply(
            r -> {
              expect(r, IrohEventKind.DOC_QUERY);
              return DocEntry.decodeList(r.data(), Integer.toUnsignedLong(r.event().flags()));
            });
  }

  /** Delete this author's entries whose keys begin with the UTF-8 prefix. */
  public CompletableFuture<Long> deleteAsync(AuthorId author, String prefix) {
    return deleteAsync(author, prefix.getBytes(StandardCharsets.UTF_8));
  }

  /** Delete this author's entries under prefix (empty means all), returning the removed count. */
  public CompletableFuture<Long> deleteAsync(AuthorId author, byte[] prefix) {
    return DataPlane.docDel(runtime, docHandle, author.hex(), prefix)
        .thenApply(
            r -> {
              expect(r, IrohEventKind.DOC_DEL);
              if (r.data().length != Long.BYTES)
                throw new IrohException("invalid deletion count payload");
              return ByteBuffer.wrap(r.data()).order(ByteOrder.LITTLE_ENDIAN).getLong();
            });
  }

  /** Read the value blob associated with query metadata. */
  public CompletableFuture<byte[]> readEntryContentAsync(BlobId hash) {
    return DataPlane.docReadEntryContent(runtime, docHandle, hash.hex())
        .thenApply(r -> expect(r, IrohEventKind.BLOB_READ).data());
  }

  /**
   * Read one local batch of content hashes, preserving input order and duplicates. Missing or
   * initially partial blobs are empty optionals; a complete zero-byte blob is present. This never
   * fetches or retains content. Store/integrity errors fail the whole batch. This is not a global
   * snapshot: use blob custody when content must survive concurrent GC.
   */
  public CompletableFuture<List<Optional<byte[]>>> readEntryContentsIfCompleteAsync(
      List<BlobId> contentHashes) {
    List<String> hashes = List.copyOf(contentHashes).stream().map(BlobId::hex).toList();
    return DataPlane.docReadEntryContentsIfComplete(runtime, docHandle, hashes)
        .thenApply(
            r ->
                decodeContentsBatch(
                    expect(r, IrohEventKind.DOC_CONTENTS_BATCH).data(), hashes.size()));
  }

  static List<Optional<byte[]>> decodeContentsBatch(byte[] payload, int expectedCount) {
    ByteBuffer input = ByteBuffer.wrap(payload).order(ByteOrder.LITTLE_ENDIAN);
    if (input.remaining() < Integer.BYTES) throw invalidBatch();
    long count = Integer.toUnsignedLong(input.getInt());
    // Each slot needs at least its presence byte. Check before allocating.
    if (count != expectedCount || count > input.remaining()) throw invalidBatch();
    List<Optional<byte[]>> values = new ArrayList<>(expectedCount);
    for (int i = 0; i < expectedCount; i++) {
      if (!input.hasRemaining()) throw invalidBatch();
      int present = Byte.toUnsignedInt(input.get());
      if (present == 0) values.add(Optional.empty());
      else if (present == 1) {
        if (input.remaining() < Long.BYTES) throw invalidBatch();
        long length = input.getLong();
        if (Long.compareUnsigned(length, input.remaining()) > 0) throw invalidBatch();
        byte[] bytes = new byte[(int) length];
        input.get(bytes);
        values.add(Optional.of(bytes));
      } else throw invalidBatch();
    }
    if (input.hasRemaining()) throw invalidBatch();
    return List.copyOf(values);
  }

  private static IrohException invalidBatch() {
    return new IrohException("invalid Docs contents batch payload");
  }

  /** Share the namespace (0=read, 1=write). */
  public CompletableFuture<String> shareAsync(int mode) {
    return share(mode, false);
  }

  /** Share the namespace with this node's address hints (0=read, 1=write). */
  public CompletableFuture<String> shareWithAddrAsync(int mode) {
    return share(mode, true);
  }

  private CompletableFuture<String> share(int mode, boolean withAddr) {
    return DataPlane.docShare(runtime, docHandle, mode, withAddr)
        .thenApply(
            r -> new String(expect(r, IrohEventKind.DOC_SHARED).data(), StandardCharsets.UTF_8));
  }

  /** Start syncing without an explicit peer list. */
  public CompletableFuture<Void> startSyncAsync() {
    return startSyncAsync(List.of());
  }

  /** Start syncing with explicit peer endpoint identifiers (64 hex characters each). */
  public CompletableFuture<Void> startSyncAsync(List<String> peers) {
    List<String> copy = List.copyOf(peers);
    if (copy.stream().anyMatch(p -> !p.matches("[0-9a-fA-F]{64}")))
      return CompletableFuture.failedFuture(
          new IllegalArgumentException("peer must be a 64-character hex identifier"));
    return DataPlane.docStartSync(runtime, docHandle, copy)
        .thenApply(
            r -> {
              expect(r, IrohEventKind.UNIT_RESULT);
              return null;
            });
  }

  /** Stop syncing this namespace. */
  public CompletableFuture<Void> leaveAsync() {
    return DataPlane.docLeave(runtime, docHandle)
        .thenApply(
            r -> {
              expect(r, IrohEventKind.UNIT_RESULT);
              return null;
            });
  }

  // ── ABI 3.3 data plane ──────────────────────────────────────────────────

  /**
   * Report the authority this node holds over <b>this</b> document's namespace. Needs a document
   * obtained from {@link Docs} (which records the owning node).
   */
  public CompletableFuture<AuthorityReport> authorityReportAsync() {
    if (nodeHandle == 0) {
      return CompletableFuture.failedFuture(
          new IllegalStateException("document has no owning node handle"));
    }
    return DataPlane.docAuthorityReport(runtime, nodeHandle, docHandle)
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.DOC_AUTHORITY_REPORT) {
                throw new IrohException("unexpected event " + r.event().kind());
              }
              return AuthorityReport.parse(r.event().flags(), r.data());
            });
  }

  /**
   * Write {@code key = value} after committing the blob metadata to disk: the crash-durable
   * publication boundary.
   *
   * @return the content hash
   */
  public CompletableFuture<BlobId> setBytesDurableAsync(AuthorId author, byte[] key, byte[] value) {
    return DataPlane.docSetBytesDurable(runtime, docHandle, author.hex(), key, value)
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.DOC_SET) {
                throw new IrohException("unexpected event " + r.event().kind());
              }
              return BlobId.of(new String(r.data(), StandardCharsets.UTF_8));
            });
  }

  /**
   * Whether an entry exists for exactly {@code (author, key)}, <b>including</b> a deletion
   * tombstone.
   */
  public CompletableFuture<Boolean> entryExistsAsync(AuthorId author, byte[] key) {
    return DataPlane.docEntryExists(runtime, docHandle, author.hex(), key)
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.DOC_ENTRY_EXISTS) {
                throw new IrohException("unexpected event " + r.event().kind());
              }
              return r.event().flags() != 0;
            });
  }

  /** Local storage status of an entry's value blob, and its size in bytes. */
  public record ContentStatus(site.aster.blobs.BlobStatus status, long size) {}

  /** Local storage status of the value blob {@code contentHash}. */
  public CompletableFuture<ContentStatus> entryContentStatusAsync(BlobId contentHash) {
    return DataPlane.docEntryContentStatus(runtime, docHandle, contentHash.hex())
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.DOC_CONTENT_STATUS) {
                throw new IrohException("unexpected event " + r.event().kind());
              }
              long size =
                  java.nio.ByteBuffer.wrap(r.data())
                      .order(java.nio.ByteOrder.LITTLE_ENDIAN)
                      .getLong();
              return new ContentStatus(
                  site.aster.blobs.BlobStatus.fromCode(r.event().flags()), size);
            });
  }

  /**
   * Release both references of this document (live sync and the handle) under one deadline so its
   * namespace can change role. The handle is freed immediately; this object is unusable afterwards.
   */
  public CompletableFuture<Void> shutdownForRoleSwapAsync(java.time.Duration budget) {
    return DataPlane.docShutdownForRoleSwap(runtime, docHandle, budget.toMillis())
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.UNIT_RESULT) {
                throw new IrohException("unexpected event " + r.event().kind());
              }
              return null;
            });
  }
}
