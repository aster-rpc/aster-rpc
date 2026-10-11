package site.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import site.aster.event.IrohEvent;
import site.aster.handle.IrohRuntime;

/**
 * Low-level access to the C ABI 3.3 data-plane functions ({@code ffi/src/data.rs}): multi-provider
 * blob fetch, ranged reads, tag prefix delete, GC, docs authority / retire / open / import, durable
 * writes, connection paths and send-stream priority.
 *
 * <p>The idiomatic wrappers live on {@code IrohBlobs}, {@code IrohTags}, {@code Docs}, {@code Doc},
 * {@code IrohConnection} and {@code IrohStream}; this class only builds arguments, invokes the
 * downcall and turns the completion event into a {@link Result} (payload copied, native buffer
 * released). {@code aster_bytes_t} / {@code aster_bytes_list_t} are passed by value, so the
 * platform ABI (including Windows x64, where 16-byte structs go by reference) is handled by FFM.
 */
public final class DataPlane {

  private DataPlane() {}

  /** A finished operation: the event and a copy of its payload. */
  public record Result(IrohEvent event, byte[] data) {}

  /** {@code ASTER_FETCH_TO_STORE}. */
  public static final int FETCH_TO_STORE = 1;

  public static final MemoryLayout ASTER_PATH_INFO =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("path_kind"), // 4
          ValueLayout.JAVA_INT.withName("is_selected"), // 8
          ValueLayout.JAVA_SHORT.withName("peer_port"), // 12
          ValueLayout.JAVA_SHORT.withName("_pad"), // 14
          IrohLibrary.ASTER_BYTES.withName("peer_host"), // 16
          IrohLibrary.ASTER_BYTES.withName("relay_url"), // 32
          IrohLibrary.ASTER_BYTES.withName("other"), // 48
          ValueLayout.JAVA_LONG.withName("rtt_micros")); // 64 -> 72

  public static final MemoryLayout ASTER_PATH_LIST =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("_pad"), // 4
          ValueLayout.ADDRESS.withName("items"), // 8
          ValueLayout.JAVA_LONG.withName("len"), // 16
          ValueLayout.ADDRESS.withName("storage")); // 24 -> 32

  static {
    if (ASTER_PATH_INFO.byteSize() != 72 || ASTER_PATH_LIST.byteSize() != 32) {
      throw new AssertionError("aster_path_info_t / aster_path_list_t layout drift");
    }
  }

  private static final ValueLayout J = ValueLayout.JAVA_LONG;
  private static final ValueLayout I = ValueLayout.JAVA_INT;
  private static final ValueLayout A = ValueLayout.ADDRESS;
  private static final MemoryLayout B = IrohLibrary.ASTER_BYTES;
  private static final MemoryLayout BL = IrohLibrary.ASTER_BYTES_LIST;

  private static final ConcurrentHashMap<String, MethodHandle> HANDLES = new ConcurrentHashMap<>();

  private static MethodHandle handle(String name, FunctionDescriptor desc) {
    return HANDLES.computeIfAbsent(name, n -> IrohLibrary.getInstance().getHandle(n, desc));
  }

  // ── argument helpers ──────────────────────────────────────────────────────

  /** An {@code aster_bytes_t} (by-value struct) holding a copy of {@code data}. */
  public static MemorySegment bytes(SegmentAllocator alloc, byte[] data) {
    MemorySegment b = alloc.allocate(B);
    if (data.length > 0) {
      MemorySegment heap = alloc.allocate(data.length);
      heap.copyFrom(MemorySegment.ofArray(data));
      b.set(ValueLayout.ADDRESS, 0, heap);
    } else {
      b.set(ValueLayout.ADDRESS, 0, MemorySegment.NULL);
    }
    b.set(ValueLayout.JAVA_LONG, 8, (long) data.length);
    return b;
  }

  public static MemorySegment utf8(SegmentAllocator alloc, String s) {
    return bytes(alloc, s.getBytes(StandardCharsets.UTF_8));
  }

  /** An {@code aster_bytes_list_t} of UTF-8 strings. */
  public static MemorySegment utf8List(SegmentAllocator alloc, List<String> items) {
    MemorySegment list = alloc.allocate(BL);
    if (items.isEmpty()) {
      list.set(ValueLayout.ADDRESS, 0, MemorySegment.NULL);
      list.set(ValueLayout.JAVA_LONG, 8, 0L);
      return list;
    }
    MemorySegment arr = alloc.allocate(B, items.size());
    for (int i = 0; i < items.size(); i++) {
      MemorySegment one = utf8(alloc, items.get(i));
      arr.asSlice(i * B.byteSize(), B.byteSize()).copyFrom(one);
    }
    list.set(ValueLayout.ADDRESS, 0, arr);
    list.set(ValueLayout.JAVA_LONG, 8, (long) items.size());
    return list;
  }

  // ── submission ────────────────────────────────────────────────────────────

  @FunctionalInterface
  private interface Call {
    int invoke(Arena arena, MemorySegment outOp) throws Throwable;
  }

  /**
   * Invoke an async C function and return a future of its completion. The arena is closed when the
   * call returns: every 3.3 function copies its inputs before returning.
   */
  private static CompletableFuture<Result> submit(IrohRuntime runtime, String name, Call call) {
    long opId;
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment opSeg = arena.allocate(ValueLayout.JAVA_LONG);
      int status = call.invoke(arena, opSeg);
      if (status != 0) {
        return CompletableFuture.failedFuture(
            new IrohException(IrohStatus.fromCode(status), name + " failed: " + lastError()));
      }
      opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(new IrohException(name + " threw: " + t));
    }
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            ev -> {
              try {
                byte[] data =
                    ev.dataLen() > 0 && ev.data() != null && ev.data().address() != 0
                        ? ev.data().asSlice(0, ev.dataLen()).toArray(ValueLayout.JAVA_BYTE)
                        : new byte[0];
                return new Result(ev, data);
              } finally {
                if (ev.hasBuffer()) {
                  try {
                    runtime.releaseBuffer(ev.buffer());
                  } catch (RuntimeException ignored) {
                    // Already released or the runtime is closing.
                  }
                }
              }
            });
  }

  /** The thread's last C ABI error message, or "" if none. */
  public static String lastError() {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment buf = arena.allocate(1024);
      long n =
          (long)
              handle("aster_last_error_message", FunctionDescriptor.of(J, A, J)).invoke(buf, 1023L);
      if (n <= 0) {
        return "";
      }
      return new String(
          buf.asSlice(0, Math.min(n, 1023)).toArray(ValueLayout.JAVA_BYTE), StandardCharsets.UTF_8);
    } catch (Throwable t) {
      return "";
    }
  }

  private static void check(int status, String name) {
    if (status != 0) {
      throw new IrohException(IrohStatus.fromCode(status), name + " failed: " + lastError());
    }
  }

  // ── blobs ─────────────────────────────────────────────────────────────────

  public static CompletableFuture<Result> blobsDownloadHashFrom(
      IrohRuntime rt,
      long node,
      String hashHex,
      List<String> providers,
      int format,
      int strategy,
      int flags) {
    var mh =
        handle(
            "aster_blobs_download_hash_from", FunctionDescriptor.of(I, J, J, B, BL, I, I, I, J, A));
    return submit(
        rt,
        "aster_blobs_download_hash_from",
        (a, op) ->
            (int)
                mh.invoke(
                    rt.nativeHandle(),
                    node,
                    utf8(a, hashHex),
                    utf8List(a, providers),
                    format,
                    strategy,
                    flags,
                    0L,
                    op));
  }

  public static CompletableFuture<Result> blobsDownloadHash(
      IrohRuntime rt, long node, String hashHex, String provider, int format, int flags) {
    var mh = handle("aster_blobs_download_hash", FunctionDescriptor.of(I, J, J, B, B, I, I, J, A));
    return submit(
        rt,
        "aster_blobs_download_hash",
        (a, op) ->
            (int)
                mh.invoke(
                    rt.nativeHandle(),
                    node,
                    utf8(a, hashHex),
                    utf8(a, provider),
                    format,
                    flags,
                    0L,
                    op));
  }

  public static CompletableFuture<Result> blobsReadRange(
      IrohRuntime rt, long node, String hashHex, long offset, long len) {
    var mh = handle("aster_blobs_read_range", FunctionDescriptor.of(I, J, J, B, J, J, J, A));
    return submit(
        rt,
        "aster_blobs_read_range",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), node, utf8(a, hashHex), offset, len, 0L, op));
  }

  public static CompletableFuture<Result> tagsDeletePrefix(
      IrohRuntime rt, long node, String prefix) {
    var mh = handle("aster_tags_delete_prefix", FunctionDescriptor.of(I, J, J, B, J, A));
    return submit(
        rt,
        "aster_tags_delete_prefix",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), node, utf8(a, prefix), 0L, op));
  }

  public static CompletableFuture<Result> blobsGcRunOnce(IrohRuntime rt, long node) {
    var mh = handle("aster_blobs_gc_run_once", FunctionDescriptor.of(I, J, J, J, A));
    return submit(
        rt, "aster_blobs_gc_run_once", (a, op) -> (int) mh.invoke(rt.nativeHandle(), node, 0L, op));
  }

  // ── docs ──────────────────────────────────────────────────────────────────

  private static CompletableFuture<Result> nodeAndBytes(
      String name, IrohRuntime rt, long node, byte[] arg) {
    var mh = handle(name, FunctionDescriptor.of(I, J, J, B, J, A));
    return submit(
        rt, name, (a, op) -> (int) mh.invoke(rt.nativeHandle(), node, bytes(a, arg), 0L, op));
  }

  public static CompletableFuture<Result> docsAuthorityReport(
      IrohRuntime rt, long node, String namespaceHex) {
    return nodeAndBytes(
        "aster_docs_authority_report", rt, node, namespaceHex.getBytes(StandardCharsets.UTF_8));
  }

  public static CompletableFuture<Result> docsRetireLocalWriteAuthority(
      IrohRuntime rt, long node, String namespaceHex) {
    return nodeAndBytes(
        "aster_docs_retire_local_write_authority",
        rt,
        node,
        namespaceHex.getBytes(StandardCharsets.UTF_8));
  }

  public static CompletableFuture<Result> docsCreate(IrohRuntime rt, long node) {
    var mh = handle("aster_docs_create", FunctionDescriptor.of(I, J, J, J, A));
    return submit(
        rt, "aster_docs_create", (a, op) -> (int) mh.invoke(rt.nativeHandle(), node, 0L, op));
  }

  public static CompletableFuture<Result> docsCreateAuthor(IrohRuntime rt, long node) {
    var mh = handle("aster_docs_create_author", FunctionDescriptor.of(I, J, J, J, A));
    return submit(
        rt,
        "aster_docs_create_author",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), node, 0L, op));
  }

  public static CompletableFuture<Result> docsJoin(
      IrohRuntime rt, long node, String ticket, boolean subscribe) {
    String name = subscribe ? "aster_docs_join_and_subscribe" : "aster_docs_join";
    var mh = handle(name, FunctionDescriptor.of(I, J, J, B, J, A));
    return submit(
        rt, name, (a, op) -> (int) mh.invoke(rt.nativeHandle(), node, utf8(a, ticket), 0L, op));
  }

  public static CompletableFuture<Result> docSetBytes(
      IrohRuntime rt, long doc, String author, byte[] key, byte[] value) {
    var mh = handle("aster_doc_set_bytes", FunctionDescriptor.of(I, J, J, B, B, B, J, A));
    return submit(
        rt,
        "aster_doc_set_bytes",
        (a, op) ->
            (int)
                mh.invoke(
                    rt.nativeHandle(),
                    doc,
                    utf8(a, author),
                    bytes(a, key),
                    bytes(a, value),
                    0L,
                    op));
  }

  public static CompletableFuture<Result> docGetExact(
      IrohRuntime rt, long doc, String author, byte[] key) {
    var mh = handle("aster_doc_get_exact", FunctionDescriptor.of(I, J, J, B, B, J, A));
    return submit(
        rt,
        "aster_doc_get_exact",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), doc, utf8(a, author), bytes(a, key), 0L, op));
  }

  public static CompletableFuture<Result> docQuery(
      IrohRuntime rt, long doc, int mode, byte[] key, long limit) {
    var mh = handle("aster_doc_query", FunctionDescriptor.of(I, J, J, I, B, J, J, A));
    return submit(
        rt,
        "aster_doc_query",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), doc, mode, bytes(a, key), limit, 0L, op));
  }

  public static CompletableFuture<Result> docDel(
      IrohRuntime rt, long doc, String author, byte[] prefix) {
    var mh = handle("aster_doc_del", FunctionDescriptor.of(I, J, J, B, B, J, A));
    return submit(
        rt,
        "aster_doc_del",
        (a, op) ->
            (int) mh.invoke(rt.nativeHandle(), doc, utf8(a, author), bytes(a, prefix), 0L, op));
  }

  public static CompletableFuture<Result> docReadEntryContent(
      IrohRuntime rt, long doc, String hash) {
    var mh = handle("aster_doc_read_entry_content", FunctionDescriptor.of(I, J, J, B, J, A));
    return submit(
        rt,
        "aster_doc_read_entry_content",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), doc, utf8(a, hash), 0L, op));
  }

  /** ABI 4.14: one local batch read, preserving order and duplicates. */
  public static CompletableFuture<Result> docReadEntryContentsIfComplete(
      IrohRuntime rt, long doc, List<String> hashes) {
    var mh =
        handle(
            "aster_doc_read_entry_contents_if_complete", FunctionDescriptor.of(I, J, J, BL, J, A));
    return submit(
        rt,
        "aster_doc_read_entry_contents_if_complete",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), doc, utf8List(a, hashes), 0L, op));
  }

  public static CompletableFuture<Result> docShare(
      IrohRuntime rt, long doc, int mode, boolean withAddr) {
    String name = withAddr ? "aster_doc_share_with_addr" : "aster_doc_share";
    var mh = handle(name, FunctionDescriptor.of(I, J, J, I, J, A));
    return submit(rt, name, (a, op) -> (int) mh.invoke(rt.nativeHandle(), doc, mode, 0L, op));
  }

  public static CompletableFuture<Result> docStartSync(
      IrohRuntime rt, long doc, List<String> peers) {
    var mh = handle("aster_doc_start_sync", FunctionDescriptor.of(I, J, J, BL, J, A));
    return submit(
        rt,
        "aster_doc_start_sync",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), doc, utf8List(a, peers), 0L, op));
  }

  public static CompletableFuture<Result> docLeave(IrohRuntime rt, long doc) {
    var mh = handle("aster_doc_leave", FunctionDescriptor.of(I, J, J, J, A));
    return submit(
        rt, "aster_doc_leave", (a, op) -> (int) mh.invoke(rt.nativeHandle(), doc, 0L, op));
  }

  public static CompletableFuture<Result> docsOpen(IrohRuntime rt, long node, String namespaceHex) {
    return nodeAndBytes("aster_docs_open", rt, node, namespaceHex.getBytes(StandardCharsets.UTF_8));
  }

  public static CompletableFuture<Result> docsImportWriteNamespace(
      IrohRuntime rt, long node, byte[] secret) {
    return nodeAndBytes("aster_docs_import_write_namespace", rt, node, secret);
  }

  public static CompletableFuture<Result> docsImportReadNamespace(
      IrohRuntime rt, long node, String namespaceHex) {
    return nodeAndBytes(
        "aster_docs_import_read_namespace",
        rt,
        node,
        namespaceHex.getBytes(StandardCharsets.UTF_8));
  }

  public static CompletableFuture<Result> docAuthorityReport(IrohRuntime rt, long node, long doc) {
    var mh = handle("aster_doc_authority_report", FunctionDescriptor.of(I, J, J, J, J, A));
    return submit(
        rt,
        "aster_doc_authority_report",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), node, doc, 0L, op));
  }

  /** Synchronous, pure: the 32-byte authority fingerprint of (namespace, access code). */
  public static byte[] docAuthorityFingerprint(String namespaceHex, int access) {
    var mh = handle("aster_doc_authority_fingerprint", FunctionDescriptor.of(I, B, I, A));
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(32);
      int status = (int) mh.invoke(utf8(arena, namespaceHex), access, out);
      check(status, "aster_doc_authority_fingerprint");
      return out.toArray(ValueLayout.JAVA_BYTE);
    } catch (IrohException e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException("aster_doc_authority_fingerprint threw: " + t);
    }
  }

  public static CompletableFuture<Result> docShutdownForRoleSwap(
      IrohRuntime rt, long doc, long budgetMs) {
    var mh = handle("aster_doc_shutdown_for_role_swap", FunctionDescriptor.of(I, J, J, J, J, A));
    return submit(
        rt,
        "aster_doc_shutdown_for_role_swap",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), doc, budgetMs, 0L, op));
  }

  public static CompletableFuture<Result> docSetBytesDurable(
      IrohRuntime rt, long doc, String authorHex, byte[] key, byte[] value) {
    var mh = handle("aster_doc_set_bytes_durable", FunctionDescriptor.of(I, J, J, B, B, B, J, A));
    return submit(
        rt,
        "aster_doc_set_bytes_durable",
        (a, op) ->
            (int)
                mh.invoke(
                    rt.nativeHandle(),
                    doc,
                    utf8(a, authorHex),
                    bytes(a, key),
                    bytes(a, value),
                    0L,
                    op));
  }

  public static CompletableFuture<Result> docEntryExists(
      IrohRuntime rt, long doc, String authorHex, byte[] key) {
    var mh = handle("aster_doc_entry_exists", FunctionDescriptor.of(I, J, J, B, B, J, A));
    return submit(
        rt,
        "aster_doc_entry_exists",
        (a, op) ->
            (int) mh.invoke(rt.nativeHandle(), doc, utf8(a, authorHex), bytes(a, key), 0L, op));
  }

  public static CompletableFuture<Result> docEntryContentStatus(
      IrohRuntime rt, long doc, String contentHashHex) {
    var mh = handle("aster_doc_entry_content_status", FunctionDescriptor.of(I, J, J, B, J, A));
    return submit(
        rt,
        "aster_doc_entry_content_status",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), doc, utf8(a, contentHashHex), 0L, op));
  }

  // ── net ───────────────────────────────────────────────────────────────────

  /** One decoded {@code aster_path_info_t}. */
  public record RawPath(
      int kind,
      boolean selected,
      String peerHost,
      int peerPort,
      String relayUrl,
      String other,
      long rttMicros) {}

  private static String readBytes(MemorySegment item, long offset) {
    MemorySegment ptr = item.get(ValueLayout.ADDRESS, offset);
    long len = item.get(ValueLayout.JAVA_LONG, offset + 8);
    if (len == 0 || ptr.address() == 0) {
      return null;
    }
    return new String(ptr.reinterpret(len).toArray(ValueLayout.JAVA_BYTE), StandardCharsets.UTF_8);
  }

  /** {@code aster_connection_paths} / {@code aster_connection_selected_path}, decoded. */
  public static List<RawPath> connectionPaths(IrohRuntime rt, long connection, boolean selected) {
    String name = selected ? "aster_connection_selected_path" : "aster_connection_paths";
    var mh = handle(name, FunctionDescriptor.of(I, J, J, A));
    var release = handle("aster_path_list_release", FunctionDescriptor.of(I, A));
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment list = arena.allocate(ASTER_PATH_LIST);
      int status = (int) mh.invoke(rt.nativeHandle(), connection, list);
      check(status, name);
      try {
        long len = list.get(ValueLayout.JAVA_LONG, 16);
        MemorySegment items =
            list.get(ValueLayout.ADDRESS, 8).reinterpret(len * ASTER_PATH_INFO.byteSize());
        var out = new java.util.ArrayList<RawPath>((int) len);
        for (long i = 0; i < len; i++) {
          MemorySegment it =
              items.asSlice(i * ASTER_PATH_INFO.byteSize(), ASTER_PATH_INFO.byteSize());
          out.add(
              new RawPath(
                  it.get(ValueLayout.JAVA_INT, 4),
                  it.get(ValueLayout.JAVA_INT, 8) != 0,
                  readBytes(it, 16),
                  Short.toUnsignedInt(it.get(ValueLayout.JAVA_SHORT, 12)),
                  readBytes(it, 32),
                  readBytes(it, 48),
                  it.get(ValueLayout.JAVA_LONG, 64)));
        }
        return List.copyOf(out);
      } finally {
        int unused = (int) release.invoke(list);
      }
    } catch (IrohException e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException(name + " threw: " + t);
    }
  }

  public static CompletableFuture<Result> sendStreamSetPriority(
      IrohRuntime rt, long stream, int priority) {
    var mh = handle("aster_send_stream_set_priority", FunctionDescriptor.of(I, J, J, I, J, A));
    return submit(
        rt,
        "aster_send_stream_set_priority",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), stream, priority, 0L, op));
  }

  public static CompletableFuture<Result> sendStreamPriority(IrohRuntime rt, long stream) {
    var mh = handle("aster_send_stream_priority", FunctionDescriptor.of(I, J, J, J, A));
    return submit(
        rt,
        "aster_send_stream_priority",
        (a, op) -> (int) mh.invoke(rt.nativeHandle(), stream, 0L, op));
  }

  /**
   * {@code aster_add_node_addr} on a node (accepted since ABI 3.3): make {@code endpointId}
   * reachable at {@code directAddresses} / {@code relayUrl} for later dials and fetches.
   */
  public static void addNodeAddr(
      IrohRuntime rt, long node, String endpointId, String relayUrl, List<String> direct) {
    var addrLayout = IrohLibrary.ASTER_NODE_ADDR;
    var mh = handle("aster_add_node_addr", FunctionDescriptor.of(I, J, J, addrLayout));
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment addr = arena.allocate(addrLayout);
      addr.asSlice(0, 16).copyFrom(utf8(arena, endpointId));
      addr.asSlice(16, 16).copyFrom(utf8(arena, relayUrl == null ? "" : relayUrl));
      addr.asSlice(32, 16).copyFrom(utf8List(arena, direct));
      int status = (int) mh.invoke(rt.nativeHandle(), node, addr);
      check(status, "aster_add_node_addr");
    } catch (IrohException e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException("aster_add_node_addr threw: " + t);
    }
  }
}
