package site.aster.blobs;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.List;
import java.util.Objects;
import java.util.function.Function;
import site.aster.event.IrohEvent;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.ffi.NativeCall;
import site.aster.handle.IrohRuntime;

/** Temporary Raw/HashSeq GC custody without persistent anonymous roots (ABI 4.13). */
public final class BlobCustody implements AutoCloseable {
  private static final ValueLayout.OfLong J = ValueLayout.JAVA_LONG;
  private static final ValueLayout.OfInt I = ValueLayout.JAVA_INT;
  private static final java.lang.foreign.AddressLayout A = ValueLayout.ADDRESS;
  private static final MethodHandle NEW = h("aster_blobs_custody_new", J, J, A);
  private static final MethodHandle FREE = h("aster_blob_custody_free", J, J);
  private static final MethodHandle PROTECT =
      h("aster_blob_custody_protect", J, J, NativeCall.BYTES, I, J, A);
  private static final MethodHandle ADD =
      h("aster_blob_custody_add_bytes", J, J, NativeCall.BYTES, I, J, A);
  private static final MethodHandle PATH =
      h("aster_blob_custody_add_path", J, J, NativeCall.BYTES, I, J, A);
  private static final MethodHandle RETAIN =
      h("aster_blob_custody_retain", J, J, NativeCall.BYTES, I, NativeCall.BYTES, J, A);
  private static final MethodHandle READ =
      h("aster_blob_custody_read", J, J, NativeCall.BYTES, J, A);
  private static final MethodHandle DOWNLOAD =
      h(
          "aster_blob_custody_download_hash_from",
          J,
          J,
          NativeCall.BYTES,
          IrohLibrary.ASTER_BYTES_LIST,
          I,
          I,
          I,
          J,
          A);
  private static final MethodHandle VERIFIED =
      h(
          "aster_blobs_add_bytes_verified",
          J,
          J,
          NativeCall.BYTES,
          NativeCall.BYTES,
          I,
          NativeCall.BYTES,
          J,
          A);
  private static final MethodHandle VERIFIED_PATH =
      h(
          "aster_blobs_add_path_verified",
          J,
          J,
          NativeCall.BYTES,
          NativeCall.BYTES,
          I,
          NativeCall.BYTES,
          J,
          A);

  private static MethodHandle h(String name, java.lang.foreign.MemoryLayout... args) {
    return NativeCall.handle(name, FunctionDescriptor.of(ValueLayout.JAVA_INT, args));
  }

  private final IrohRuntime runtime;
  private long handle;

  private BlobCustody(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
  }

  static BlobCustody create(IrohRuntime runtime, long node) {
    try (Arena arena = Arena.ofConfined()) {
      var out = arena.allocate(J);
      NativeCall.callChecked(NEW, "create blob custody", runtime.nativeHandle(), node, out);
      return new BlobCustody(runtime, out.get(J, 0));
    }
  }

  @FunctionalInterface
  private interface Submit {
    int call(Arena arena, MemorySegment operation);
  }

  private synchronized <T> BlobOperation<T> submit(Submit call, Function<IrohEvent, T> decode) {
    if (handle == 0) throw new IllegalStateException("blob custody is closed");
    return start(runtime, call, decode);
  }

  private static <T> BlobOperation<T> start(
      IrohRuntime runtime, Submit call, Function<IrohEvent, T> decode) {
    try (Arena arena = Arena.ofConfined()) {
      var out = arena.allocate(J);
      NativeCall.check(call.call(arena, out), "blob operation submission");
      return new BlobOperation<>(runtime, out.get(J, 0), decode);
    }
  }

  private static void expected(IrohEvent event, IrohEventKind kind) {
    if (event.kind() != kind) throw new IrohException("unexpected blob event " + event.kind());
  }

  private static BlobId added(IrohEvent event) {
    expected(event, IrohEventKind.BLOB_ADDED);
    return BlobId.of(new String(BlobOperation.payload(event), StandardCharsets.UTF_8));
  }

  private static Void done(IrohEvent event) {
    expected(event, IrohEventKind.BLOB_CUSTODY_DONE);
    return null;
  }

  private static int format(BlobFormat format) {
    return Objects.requireNonNull(format, "format").code;
  }

  /**
   * Protect even an absent root before fetching it. HashSeq protection includes its child graph.
   */
  public BlobOperation<Void> protectAsync(BlobId hash, BlobFormat format) {
    int code = format(format);
    return submit(
        (a, o) ->
            NativeCall.call(
                PROTECT,
                runtime.nativeHandle(),
                handle,
                NativeCall.string(a, hash.hex()),
                code,
                0L,
                o),
        BlobCustody::done);
  }

  public BlobOperation<BlobId> addBytesAsync(byte[] data, BlobFormat format) {
    Objects.requireNonNull(data, "data");
    int code = format(format);
    return submit(
        (a, o) ->
            NativeCall.call(
                ADD, runtime.nativeHandle(), handle, NativeCall.bytes(a, data), code, 0L, o),
        BlobCustody::added);
  }

  /** Copy file bytes; no mutable source reference or persistent anonymous tag is retained. */
  public BlobOperation<BlobId> addPathAsync(Path path, BlobFormat format) {
    Objects.requireNonNull(path, "path");
    int code = format(format);
    return submit(
        (a, o) ->
            NativeCall.call(
                PATH,
                runtime.nativeHandle(),
                handle,
                NativeCall.string(a, path.toString()),
                code,
                0L,
                o),
        BlobCustody::added);
  }

  /**
   * Atomically replace a named root held with this exact format. This does not add an fsync
   * guarantee.
   */
  public BlobOperation<Void> retainAsync(BlobId hash, BlobFormat format, String tagName) {
    Objects.requireNonNull(tagName, "tagName");
    int code = format(format);
    return submit(
        (a, o) ->
            NativeCall.call(
                RETAIN,
                runtime.nativeHandle(),
                handle,
                NativeCall.string(a, hash.hex()),
                code,
                NativeCall.string(a, tagName),
                0L,
                o),
        BlobCustody::done);
  }

  /** Read root bytes without interpreting HashSeq as a Collection. */
  public BlobOperation<byte[]> readAsync(BlobId hash) {
    return submit(
        (a, o) ->
            NativeCall.call(
                READ, runtime.nativeHandle(), handle, NativeCall.string(a, hash.hex()), 0L, o),
        e -> {
          expected(e, IrohEventKind.BLOB_READ);
          return BlobOperation.payload(e);
        });
  }

  /** Fetch under custody. Cancellation may wait for a stalled native transfer to finish safely. */
  public BlobOperation<FetchResult> downloadHashFromAsync(
      BlobId hash,
      List<String> providers,
      BlobFormat format,
      FetchStrategy strategy,
      boolean toStore) {
    Objects.requireNonNull(providers, "providers");
    Objects.requireNonNull(strategy, "strategy");
    int code = format(format);
    return submit(
        (a, o) ->
            NativeCall.call(
                DOWNLOAD,
                runtime.nativeHandle(),
                handle,
                NativeCall.string(a, hash.hex()),
                DataPlane.utf8List(a, providers),
                code,
                strategy.code,
                toStore ? DataPlane.FETCH_TO_STORE : 0,
                0L,
                o),
        e -> {
          expected(e, IrohEventKind.BLOB_FETCHED);
          return FetchResult.parse(BlobOperation.payload(e));
        });
  }

  static BlobOperation<BlobId> verified(
      IrohRuntime runtime,
      long node,
      byte[] data,
      Path path,
      BlobId expectedHash,
      BlobFormat format,
      String tagName) {
    Objects.requireNonNull(expectedHash, "expectedHash");
    Objects.requireNonNull(tagName, "tagName");
    int code = format(format);
    return start(
        runtime,
        (a, o) ->
            NativeCall.call(
                path == null ? VERIFIED : VERIFIED_PATH,
                runtime.nativeHandle(),
                node,
                path == null
                    ? NativeCall.bytes(a, Objects.requireNonNull(data, "data"))
                    : NativeCall.string(a, path.toString()),
                NativeCall.string(a, expectedHash.hex()),
                code,
                NativeCall.string(a, tagName),
                0L,
                o),
        BlobCustody::added);
  }

  /**
   * Seal admission. Already accepted operations keep all scope guards until their last lease ends.
   */
  @Override
  public synchronized void close() {
    if (handle == 0) return;
    int status = NativeCall.call(FREE, runtime.nativeHandle(), handle);
    if (status != IrohStatus.NOT_FOUND.code && status != IrohStatus.ALREADY_CLOSED.code)
      NativeCall.check(status, "close blob custody");
    handle = 0;
  }
}
