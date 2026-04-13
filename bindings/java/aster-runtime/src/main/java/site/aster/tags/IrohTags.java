package site.aster.tags;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import site.aster.blobs.BlobId;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;

/**
 * Tag operations for an Iroh node.
 *
 * <p>Tags provide named references to blobs in the store. They protect blobs from garbage
 * collection and allow organizing content.
 *
 * <p>Get an instance via {@link site.aster.node.IrohNode#tags}.
 */
public class IrohTags {

  private final IrohRuntime runtime;
  private final long nodeHandle;

  public IrohTags(IrohRuntime runtime, long nodeHandle) {
    this.runtime = runtime;
    this.nodeHandle = nodeHandle;
  }

  private IrohRuntime runtime() {
    return runtime;
  }

  private long nodeHandle() {
    return nodeHandle;
  }

  private MemorySegment toStringSegment(String str, SegmentAllocator alloc) {
    byte[] bytes = str.getBytes(StandardCharsets.UTF_8);
    MemorySegment seg = alloc.allocate(bytes.length);
    seg.copyFrom(MemorySegment.ofArray(bytes));
    return seg;
  }

  /**
   * Set a named tag for a blob.
   *
   * @param name the tag name
   * @param hash the blob hash
   * @param format the blob format (RAW or HASH_SEQ)
   * @return a future that completes when the tag is set
   */
  public CompletableFuture<Void> setAsync(String name, BlobId hash, TagFormat format) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] nameBytes = name.getBytes(StandardCharsets.UTF_8);
    byte[] hashBytes = hash.hex().getBytes(StandardCharsets.UTF_8);

    var nameSeg = alloc.allocate(nameBytes.length);
    nameSeg.copyFrom(MemorySegment.ofArray(nameBytes));

    var hashSeg = alloc.allocate(hashBytes.length);
    hashSeg.copyFrom(MemorySegment.ofArray(hashBytes));

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.tagsSet(
              runtime.nativeHandle(),
              nodeHandle,
              nameSeg,
              nameBytes.length,
              hashSeg,
              hashBytes.length,
              format.code(),
              0L,
              opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_tags_set failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_tags_set threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime.registry().register(opId).thenApply(event -> null);
  }

  /**
   * Get a tag by name.
   *
   * @param name the tag name
   * @return a future that completes with the tag entry, or null if not found
   */
  public CompletableFuture<TagEntry> getAsync(String name) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] nameBytes = name.getBytes(StandardCharsets.UTF_8);
    var nameSeg = alloc.allocate(nameBytes.length);
    nameSeg.copyFrom(MemorySegment.ofArray(nameBytes));

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.tagsGet(runtime.nativeHandle(), nodeHandle, nameSeg, nameBytes.length, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_tags_get failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_tags_get threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.TAG_GET) {
                if (event.status() == IrohStatus.NOT_FOUND.code) {
                  return null;
                }
                return parseTagEntry(event.data().asByteBuffer().array());
              }
              throw new IrohException("get failed: unexpected event " + event.kind());
            });
  }

  /**
   * Delete a tag by name.
   *
   * @param name the tag name
   * @return a future that completes with the number of tags deleted (0 or 1)
   */
  public CompletableFuture<Integer> deleteAsync(String name) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] nameBytes = name.getBytes(StandardCharsets.UTF_8);
    var nameSeg = alloc.allocate(nameBytes.length);
    nameSeg.copyFrom(MemorySegment.ofArray(nameBytes));

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.tagsDelete(runtime.nativeHandle(), nodeHandle, nameSeg, nameBytes.length, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_tags_delete failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_tags_delete threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.TAG_DELETED) {
                // event.flags() contains the count of deleted tags
                return event.flags();
              }
              throw new IrohException("delete failed: unexpected event " + event.kind());
            });
  }

  /**
   * List tags matching a prefix.
   *
   * @param prefix the prefix to filter by (empty string for all tags)
   * @return a future that completes with the list of matching tags
   */
  public CompletableFuture<List<TagEntry>> listPrefixAsync(String prefix) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] prefixBytes = prefix.getBytes(StandardCharsets.UTF_8);
    var prefixSeg = alloc.allocate(prefixBytes.length);
    prefixSeg.copyFrom(MemorySegment.ofArray(prefixBytes));

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.tagsListPrefix(
              runtime.nativeHandle(), nodeHandle, prefixSeg, prefixBytes.length, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_tags_list_prefix failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_tags_list_prefix threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.TAG_LIST) {
                // event.flags() contains the count
                // data contains packed tag records
                return parseTagList(event.data().asByteBuffer().array(), event.flags());
              }
              throw new IrohException("listPrefix failed: unexpected event " + event.kind());
            });
  }

  /**
   * List all tags.
   *
   * @return a future that completes with all tags
   */
  public CompletableFuture<List<TagEntry>> listAllAsync() {
    return listPrefixAsync("");
  }

  private TagEntry parseTagEntry(byte[] data) {
    // Tag format: null-terminated strings: name\0hash_hex\0format\0
    String[] parts = parseNullSeparated(data);
    if (parts.length < 3) {
      throw new IrohException("invalid tag entry data");
    }
    String name = parts[0];
    String hashHex = parts[1];
    String formatStr = parts[2];
    TagFormat format = formatStr.equals("hash_seq") ? TagFormat.HASH_SEQ : TagFormat.RAW;
    return new TagEntry(name, BlobId.of(hashHex), format);
  }

  private List<TagEntry> parseTagList(byte[] data, int count) {
    List<TagEntry> entries = new ArrayList<>();
    if (count == 0 || data.length == 0) {
      return entries;
    }
    String[] entries_str = parseNullSeparated(data);
    for (int i = 0; i < count && i < entries_str.length; i++) {
      // Each entry is: name\0hash_hex\0format\0
      // But since entries_str already split by null, we need to re-split
      // The data is actually packed as: name\0hash_hex\0format\0name\0hash_hex\0format\0...
    }
    // Simpler: just split the whole thing by null and process in groups of 3
    List<String> parts = new ArrayList<>();
    for (String s : entries_str) {
      if (!s.isEmpty()) {
        parts.add(s);
      }
    }
    for (int i = 0; i + 2 < parts.size(); i += 3) {
      try {
        String name = parts.get(i);
        String hashHex = parts.get(i + 1);
        String formatStr = parts.get(i + 2);
        TagFormat format = formatStr.equals("hash_seq") ? TagFormat.HASH_SEQ : TagFormat.RAW;
        entries.add(new TagEntry(name, BlobId.of(hashHex), format));
      } catch (Exception e) {
        // Skip invalid entries
      }
    }
    return entries;
  }

  private String[] parseNullSeparated(byte[] data) {
    List<String> parts = new ArrayList<>();
    int start = 0;
    for (int i = 0; i < data.length; i++) {
      if (data[i] == 0) {
        parts.add(new String(data, start, i - start, StandardCharsets.UTF_8));
        start = i + 1;
      }
    }
    return parts.toArray(new String[0]);
  }
}
