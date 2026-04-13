package site.aster.blobs;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;

/**
 * Blob storage operations for an Iroh node.
 *
 * <p>Get an instance via {@link site.aster.node.IrohNode#blobs}.
 */
public class IrohBlobs {

  private final IrohRuntime runtime;
  private final long nodeHandle;

  public IrohBlobs(IrohRuntime runtime, long nodeHandle) {
    this.runtime = runtime;
    this.nodeHandle = nodeHandle;
  }

  private IrohRuntime runtime() {
    return runtime;
  }

  private long nodeHandle() {
    return nodeHandle;
  }

  private MemorySegment toHexSegment(String hex, SegmentAllocator alloc) {
    byte[] hexBytes = hex.getBytes(StandardCharsets.UTF_8);
    MemorySegment seg = alloc.allocate(hexBytes.length);
    seg.copyFrom(MemorySegment.ofArray(hexBytes));
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

  private MemorySegment toStringSegment(String str, SegmentAllocator alloc) {
    byte[] bytes = str.getBytes(StandardCharsets.UTF_8);
    MemorySegment seg = alloc.allocate(bytes.length);
    seg.copyFrom(MemorySegment.ofArray(bytes));
    return seg;
  }

  /**
   * Add bytes to the blob store.
   *
   * @param data the bytes to store
   * @return a future that completes with the blob ID
   */
  public CompletableFuture<BlobId> addBytesAsync(byte[] data) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    var dataSeg = toBytesSegment(data, alloc);
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.blobsAddBytes(runtime.nativeHandle(), nodeHandle, dataSeg, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_blobs_add_bytes failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_blobs_add_bytes threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.BLOB_ADDED) {
                // Event data contains the blob ID as a hex string
                byte[] hashBytes = event.data().asByteBuffer().array();
                String hashHex = new String(hashBytes, StandardCharsets.UTF_8);
                return BlobId.of(hashHex.trim());
              }
              throw new IrohException("addBytes failed: unexpected event " + event.kind());
            });
  }

  /**
   * Add bytes as a named entry in a collection.
   *
   * @param data the bytes to store
   * @param name the name for this entry
   * @return a future that completes with the blob ID
   */
  public CompletableFuture<BlobId> addBytesAsCollectionAsync(byte[] data, String name) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    var nameSeg = toStringSegment(name, alloc);
    var dataSeg = toBytesSegment(data, alloc);
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.blobsAddBytesAsCollection(
              runtime.nativeHandle(), nodeHandle, nameSeg, dataSeg, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_blobs_add_bytes_as_collection failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_blobs_add_bytes_as_collection threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.BLOB_ADDED) {
                byte[] hashBytes = event.data().asByteBuffer().array();
                String hashHex = new String(hashBytes, StandardCharsets.UTF_8);
                return BlobId.of(hashHex.trim());
              }
              throw new IrohException(
                  "addBytesAsCollection failed: unexpected event " + event.kind());
            });
  }

  /**
   * Add a multi-file collection.
   *
   * @param entriesJson JSON string: [[name, base64data], ...]
   * @return a future that completes with the collection blob ID
   */
  public CompletableFuture<BlobId> addCollectionAsync(String entriesJson) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    var jsonSeg = toStringSegment(entriesJson, alloc);
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.blobsAddCollection(runtime.nativeHandle(), nodeHandle, jsonSeg, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_blobs_add_collection failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_blobs_add_collection threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.BLOB_COLLECTION_ADDED) {
                byte[] hashBytes = event.data().asByteBuffer().array();
                String hashHex = new String(hashBytes, StandardCharsets.UTF_8);
                return BlobId.of(hashHex.trim());
              }
              throw new IrohException("addCollection failed: unexpected event " + event.kind());
            });
  }

  /**
   * Read blob data.
   *
   * @param hashHex the blob hash as a hex string
   * @return a future that completes with the blob data
   */
  public CompletableFuture<byte[]> readAsync(String hashHex) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    var hashSeg = toHexSegment(hashHex, alloc);
    // Wrap in iroh_bytes_t struct
    var hashBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    hashBytesSeg.set(ValueLayout.ADDRESS, 0, hashSeg);
    hashBytesSeg.set(
        ValueLayout.JAVA_LONG, 8, (long) hashHex.getBytes(StandardCharsets.UTF_8).length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.blobsRead(runtime.nativeHandle(), nodeHandle, hashBytesSeg, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_blobs_read failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_blobs_read threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.BLOB_READ) {
                byte[] data = event.data().asByteBuffer().array();
                if (event.hasBuffer()) {
                  runtime.releaseBuffer(event.buffer());
                }
                return data;
              }
              throw new IrohException("read failed: unexpected event " + event.kind());
            });
  }

  /**
   * Download a blob from a ticket.
   *
   * @param ticket the blob ticket
   * @return a future that completes with the downloaded blob ID
   */
  public CompletableFuture<BlobId> downloadAsync(BlobTicket ticket) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] ticketBytes = ticket.ticket().getBytes(StandardCharsets.UTF_8);
    var ticketSeg = alloc.allocate(ticketBytes.length);
    ticketSeg.copyFrom(MemorySegment.ofArray(ticketBytes));

    var ticketBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    ticketBytesSeg.set(ValueLayout.ADDRESS, 0, ticketSeg);
    ticketBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) ticketBytes.length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.blobsDownload(runtime.nativeHandle(), nodeHandle, ticketBytesSeg, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_blobs_download failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_blobs_download threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.BLOB_DOWNLOADED) {
                byte[] hashBytes = event.data().asByteBuffer().array();
                String hashHex = new String(hashBytes, StandardCharsets.UTF_8);
                return BlobId.of(hashHex.trim());
              }
              throw new IrohException("download failed: unexpected event " + event.kind());
            });
  }

  /**
   * Get the status of a blob.
   *
   * @param id the blob ID
   * @return the blob status
   */
  public BlobStatus status(BlobId id) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] hexBytes = id.hex().getBytes(StandardCharsets.UTF_8);
    var hexSeg = alloc.allocate(hexBytes.length);
    hexSeg.copyFrom(MemorySegment.ofArray(hexBytes));

    var statusSeg = alloc.allocate(ValueLayout.JAVA_INT);
    var sizeSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.blobsStatus(
              runtime.nativeHandle(), nodeHandle, hexSeg, hexBytes.length, statusSeg, sizeSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_blobs_status failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_blobs_status threw: " + t.getMessage());
    }

    int statusCode = statusSeg.get(ValueLayout.JAVA_INT, 0);
    return BlobStatus.fromCode(statusCode);
  }

  /**
   * Check if a blob is stored locally.
   *
   * @param id the blob ID
   * @return true if the blob is complete
   */
  public boolean has(BlobId id) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] hexBytes = id.hex().getBytes(StandardCharsets.UTF_8);
    var hexSeg = alloc.allocate(hexBytes.length);
    hexSeg.copyFrom(MemorySegment.ofArray(hexBytes));

    var hasSeg = alloc.allocate(ValueLayout.JAVA_INT);

    try {
      int status =
          lib.blobsHas(runtime.nativeHandle(), nodeHandle, hexSeg, hexBytes.length, hasSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_blobs_has failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_blobs_has threw: " + t.getMessage());
    }

    int hasCode = hasSeg.get(ValueLayout.JAVA_INT, 0);
    return hasCode == 1;
  }

  /**
   * Observe blob download completion.
   *
   * @param id the blob ID to observe
   * @return a future that completes when the blob is complete
   */
  public CompletableFuture<Void> observeCompleteAsync(BlobId id) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] hexBytes = id.hex().getBytes(StandardCharsets.UTF_8);
    var hexSeg = alloc.allocate(hexBytes.length);
    hexSeg.copyFrom(MemorySegment.ofArray(hexBytes));

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.blobsObserveComplete(
              runtime.nativeHandle(), nodeHandle, hexSeg, hexBytes.length, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_blobs_observe_complete failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_blobs_observe_complete threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime.registry().register(opId).thenApply(event -> null);
  }

  /**
   * Get a snapshot of blob download progress.
   *
   * @param id the blob ID
   * @return a map with blob status info, or empty if not found
   */
  public Map<BlobId, BlobStatus> observeSnapshot(BlobId id) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] hexBytes = id.hex().getBytes(StandardCharsets.UTF_8);
    var hexSeg = alloc.allocate(hexBytes.length);
    hexSeg.copyFrom(MemorySegment.ofArray(hexBytes));

    var completeSeg = alloc.allocate(ValueLayout.JAVA_INT);
    var sizeSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.blobsObserveSnapshot(
              runtime.nativeHandle(), nodeHandle, hexSeg, hexBytes.length, completeSeg, sizeSeg);
      if (status != 0) {
        // Not found is not an error for snapshot
        return Collections.emptyMap();
      }
    } catch (Throwable t) {
      return Collections.emptyMap();
    }

    int isComplete = completeSeg.get(ValueLayout.JAVA_INT, 0);
    Map<BlobId, BlobStatus> result = new HashMap<>();
    result.put(id, isComplete == 1 ? BlobStatus.COMPLETE : BlobStatus.PARTIAL);
    return result;
  }

  /**
   * List collection entries.
   *
   * @param hashHex the collection hash as a hex string
   * @return a future that completes with the collection
   */
  public CompletableFuture<BlobCollection> listCollectionAsync(String hashHex) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] hexBytes = hashHex.getBytes(StandardCharsets.UTF_8);
    var hexSeg = alloc.allocate(hexBytes.length);
    hexSeg.copyFrom(MemorySegment.ofArray(hexBytes));

    var hashBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    hashBytesSeg.set(ValueLayout.ADDRESS, 0, hexSeg);
    hashBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) hexBytes.length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.blobsListCollection(runtime.nativeHandle(), nodeHandle, hashBytesSeg, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_blobs_list_collection failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_blobs_list_collection threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.BLOB_READ) {
                // Parse JSON: [[name, hash, size], ...]
                return parseBlobCollection(event.data().asByteBuffer().array());
              }
              throw new IrohException("listCollection failed: unexpected event " + event.kind());
            });
  }

  private BlobCollection parseBlobCollection(byte[] jsonBytes) {
    String json = new String(jsonBytes, StandardCharsets.UTF_8);
    List<BlobEntry> entries = new ArrayList<>();

    try {
      // Simple JSON parsing for [[name,hash,size],...]
      // This is a simplified parser - a real implementation would use a JSON library
      json = json.trim();
      if (!json.startsWith("[")) {
        return new BlobCollection(entries);
      }

      // Remove outer brackets and split
      String content = json.substring(1, json.length() - 1);
      if (content.isEmpty()) {
        return new BlobCollection(entries);
      }

      // Split by ],[
      String[] pairs = splitJsonArray(content);
      for (String pair : pairs) {
        pair = pair.trim();
        if (pair.startsWith("[")) {
          pair = pair.substring(1);
        }
        if (pair.endsWith("]")) {
          pair = pair.substring(0, pair.length() - 1);
        }

        String[] parts = splitJsonArray(pair);
        if (parts.length >= 3) {
          String name = unquote(parts[0]);
          String hash = unquote(parts[1]);
          long size = Long.parseLong(parts[2].trim());
          entries.add(new BlobEntry(name, BlobId.of(hash), size));
        }
      }
    } catch (Exception e) {
      // Return empty collection on parse error
    }

    return new BlobCollection(entries);
  }

  private String[] splitJsonArray(String s) {
    List<String> result = new ArrayList<>();
    StringBuilder current = new StringBuilder();
    int depth = 0;
    boolean inString = false;

    for (int i = 0; i < s.length(); i++) {
      char c = s.charAt(i);
      if (c == '"' && (i == 0 || s.charAt(i - 1) != '\\')) {
        inString = !inString;
      }
      if (!inString) {
        if (c == '[') depth++;
        else if (c == ']') depth--;
        else if (c == ',' && depth == 0) {
          result.add(current.toString());
          current = new StringBuilder();
          continue;
        }
      }
      current.append(c);
    }
    if (current.length() > 0) {
      result.add(current.toString());
    }
    return result.toArray(new String[0]);
  }

  private String unquote(String s) {
    s = s.trim();
    if (s.startsWith("\"") && s.endsWith("\"")) {
      s = s.substring(1, s.length() - 1);
    }
    return s.replace("\\\"", "\"").replace("\\\\", "\\");
  }

  /**
   * Create a ticket for a blob.
   *
   * @param id the blob ID
   * @param format the blob format
   * @return the blob ticket
   */
  public BlobTicket createTicket(BlobId id, BlobFormat format) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] hexBytes = id.hex().getBytes(StandardCharsets.UTF_8);
    var hexSeg = alloc.allocate(hexBytes.length);
    hexSeg.copyFrom(MemorySegment.ofArray(hexBytes));

    var hashBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    hashBytesSeg.set(ValueLayout.ADDRESS, 0, hexSeg);
    hashBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) hexBytes.length);

    var bufSeg = alloc.allocate(1024);
    var lenSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.blobsCreateTicket(
              runtime.nativeHandle(), nodeHandle, hashBytesSeg, bufSeg, 1024, lenSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_blobs_create_ticket failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_blobs_create_ticket threw: " + t.getMessage());
    }

    int len = (int) lenSeg.get(ValueLayout.JAVA_LONG, 0);
    if (len == 0) {
      return BlobTicket.of("");
    }

    byte[] ticketBytes = bufSeg.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
    return BlobTicket.of(new String(ticketBytes, StandardCharsets.UTF_8));
  }

  /**
   * Create a ticket for a collection.
   *
   * @param id the collection blob ID
   * @param names the names to include in the ticket
   * @return the collection ticket
   */
  public BlobTicket createCollectionTicket(BlobId id, java.util.Set<String> names) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] hexBytes = id.hex().getBytes(StandardCharsets.UTF_8);
    var hexSeg = alloc.allocate(hexBytes.length);
    hexSeg.copyFrom(MemorySegment.ofArray(hexBytes));

    var hashBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    hashBytesSeg.set(ValueLayout.ADDRESS, 0, hexSeg);
    hashBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) hexBytes.length);

    var bufSeg = alloc.allocate(1024);
    var lenSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.blobsCreateCollectionTicket(
              runtime.nativeHandle(), nodeHandle, hashBytesSeg, bufSeg, 1024, lenSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_blobs_create_collection_ticket failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_blobs_create_collection_ticket threw: " + t.getMessage());
    }

    int len = (int) lenSeg.get(ValueLayout.JAVA_LONG, 0);
    if (len == 0) {
      return BlobTicket.of("");
    }

    byte[] ticketBytes = bufSeg.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
    return BlobTicket.of(new String(ticketBytes, StandardCharsets.UTF_8));
  }

  /**
   * Get local information about a blob.
   *
   * @param id the blob ID
   * @return blob info, or null if not found
   */
  public BlobInfo localInfo(BlobId id) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] hexBytes = id.hex().getBytes(StandardCharsets.UTF_8);
    var hexSeg = alloc.allocate(hexBytes.length);
    hexSeg.copyFrom(MemorySegment.ofArray(hexBytes));

    var completeSeg = alloc.allocate(ValueLayout.JAVA_INT);
    var localBytesSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.blobsLocalInfo(
              runtime.nativeHandle(),
              nodeHandle,
              hexSeg,
              hexBytes.length,
              completeSeg,
              localBytesSeg);
      if (status != 0) {
        return null;
      }
    } catch (Throwable t) {
      return null;
    }

    int isComplete = completeSeg.get(ValueLayout.JAVA_INT, 0);
    long localBytes = localBytesSeg.get(ValueLayout.JAVA_LONG, 0);

    return new BlobInfo(id, localBytes, isComplete == 1 ? BlobStatus.COMPLETE : BlobStatus.PARTIAL);
  }
}
