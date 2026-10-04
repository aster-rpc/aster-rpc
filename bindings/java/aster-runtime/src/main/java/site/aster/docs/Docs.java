package site.aster.docs;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.CompletableFuture;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;
import site.aster.node.IrohNode;

/**
 * Document operations for an Iroh node.
 *
 * <p>Documents provide a content-addressed store with sync capabilities. Get an instance via {@link
 * IrohNode#docs}.
 */
public class Docs {

  private final IrohRuntime runtime;
  private final long nodeHandle;

  public Docs(IrohRuntime runtime, long nodeHandle) {
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

  private MemorySegment toBytesSegment(byte[] data, SegmentAllocator alloc) {
    MemorySegment dataSeg = alloc.allocate(IrohLibrary.ASTER_BYTES);
    MemorySegment heapSeg = alloc.allocate(data.length);
    heapSeg.copyFrom(MemorySegment.ofArray(data));
    dataSeg.set(ValueLayout.ADDRESS, 0, heapSeg);
    dataSeg.set(ValueLayout.JAVA_LONG, 8, (long) data.length);
    return dataSeg;
  }

  /**
   * Create a new document.
   *
   * @return a future that completes with the new document
   */
  public CompletableFuture<Doc> createAsync() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.docsCreate(runtime.nativeHandle(), nodeHandle, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "aster_docs_create failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("aster_docs_create threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.DOC_CREATED) {
                long docHandle = event.handle();
                return new Doc(runtime, docHandle, nodeHandle);
              }
              throw new IrohException("create failed: unexpected event " + event.kind());
            });
  }

  /**
   * Create a new author.
   *
   * @return a future that completes with the new author ID
   */
  public CompletableFuture<AuthorId> createAuthorAsync() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.docsCreateAuthor(runtime.nativeHandle(), nodeHandle, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "aster_docs_create_author failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("aster_docs_create_author threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.AUTHOR_CREATED) {
                byte[] authorBytes =
                    event.dataLen() > 0
                        ? event.data().asSlice(0, event.dataLen()).toArray(ValueLayout.JAVA_BYTE)
                        : new byte[0];
                if (event.hasBuffer()) {
                  runtime.releaseBuffer(event.buffer());
                }
                String authorHex = new String(authorBytes, StandardCharsets.UTF_8).trim();
                return AuthorId.of(authorHex);
              }
              throw new IrohException("createAuthor failed: unexpected event " + event.kind());
            });
  }

  /**
   * Join a document from a ticket.
   *
   * @param ticket the document ticket
   * @return a future that completes with the document
   */
  public CompletableFuture<Doc> joinAsync(String ticket) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] ticketBytes = ticket.getBytes(StandardCharsets.UTF_8);
    var ticketSeg = alloc.allocate(ticketBytes.length);
    ticketSeg.copyFrom(MemorySegment.ofArray(ticketBytes));

    var ticketBytesSeg = alloc.allocate(IrohLibrary.ASTER_BYTES);
    ticketBytesSeg.set(ValueLayout.ADDRESS, 0, ticketSeg);
    ticketBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) ticketBytes.length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.docsJoin(runtime.nativeHandle(), nodeHandle, ticketBytesSeg, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "aster_docs_join failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("aster_docs_join threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.DOC_JOINED) {
                long docHandle = event.handle();
                return new Doc(runtime, docHandle, nodeHandle);
              }
              throw new IrohException("join failed: unexpected event " + event.kind());
            });
  }

  /**
   * Join and subscribe to a document atomically.
   *
   * @param ticket the document ticket
   * @return a future that completes with the document
   */
  public CompletableFuture<Doc> joinAndSubscribeAsync(String ticket) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    byte[] ticketBytes = ticket.getBytes(StandardCharsets.UTF_8);
    var ticketSeg = alloc.allocate(ticketBytes.length);
    ticketSeg.copyFrom(MemorySegment.ofArray(ticketBytes));

    var ticketBytesSeg = alloc.allocate(IrohLibrary.ASTER_BYTES);
    ticketBytesSeg.set(ValueLayout.ADDRESS, 0, ticketSeg);
    ticketBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) ticketBytes.length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.docsJoinAndSubscribe(runtime.nativeHandle(), nodeHandle, ticketBytesSeg, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "aster_docs_join_and_subscribe failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("aster_docs_join_and_subscribe threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.DOC_JOINED_AND_SUBSCRIBED) {
                long docHandle = event.handle();
                return new Doc(runtime, docHandle, nodeHandle);
              }
              throw new IrohException("joinAndSubscribe failed: unexpected event " + event.kind());
            });
  }

  // ── ABI 3.3 data plane ──────────────────────────────────────────────────

  private Doc opened(DataPlane.Result r) {
    if (r.event().kind() != IrohEventKind.DOC_OPENED) {
      throw new IrohException("unexpected event " + r.event().kind());
    }
    return new Doc(runtime, r.event().handle(), nodeHandle);
  }

  /**
   * Report the authority this node holds over the namespace {@code namespaceId} names, read back
   * from the store. Fails if the namespace is not in the local store.
   */
  public CompletableFuture<AuthorityReport> authorityReportAsync(String namespaceId) {
    return DataPlane.docsAuthorityReport(runtime, nodeHandle, namespaceId)
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.DOC_AUTHORITY_REPORT) {
                throw new IrohException("unexpected event " + r.event().kind());
              }
              return AuthorityReport.parse(r.event().flags(), r.data());
            });
  }

  /** The authority fingerprint of {@code (namespaceId, access)}, as an authority report carries. */
  public static byte[] authorityFingerprint(String namespaceId, LocalAccess access) {
    return DataPlane.docAuthorityFingerprint(namespaceId, access.code);
  }

  /**
   * Retire this node's <b>local</b> write authority over a namespace, keeping every entry. Release
   * every reference first ({@link Doc#shutdownForRoleSwapAsync}); otherwise the outcome is busy.
   */
  public CompletableFuture<RetireOutcome> retireLocalWriteAuthorityAsync(String namespaceId) {
    return DataPlane.docsRetireLocalWriteAuthority(runtime, nodeHandle, namespaceId)
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.DOC_RETIRE_OUTCOME) {
                throw new IrohException("unexpected event " + r.event().kind());
              }
              var b = java.nio.ByteBuffer.wrap(r.data()).order(java.nio.ByteOrder.LITTLE_ENDIAN);
              int access = b.getInt(0);
              long handles = b.getLong(8);
              boolean retired = r.event().flags() == 0;
              return new RetireOutcome(
                  retired, retired ? LocalAccess.fromCode(access) : null, handles);
            });
  }

  /** Reopen a namespace already in the local replica; empty if it is not present. */
  public CompletableFuture<java.util.Optional<Doc>> openAsync(String namespaceId) {
    return DataPlane.docsOpen(runtime, nodeHandle, namespaceId)
        .thenApply(
            r -> {
              if (r.event().kind() == IrohEventKind.DOC_OPENED
                  && r.event().status() == IrohStatus.NOT_FOUND.code) {
                return java.util.Optional.empty();
              }
              return java.util.Optional.of(opened(r));
            });
  }

  /**
   * Open-or-import a writable namespace from its 32-byte secret. Idempotent; upgrades a prior
   * read-only import.
   */
  public CompletableFuture<Doc> importWriteNamespaceAsync(byte[] secret) {
    return DataPlane.docsImportWriteNamespace(runtime, nodeHandle, secret).thenApply(this::opened);
  }

  /**
   * Open-or-import a read-only namespace from its id. Idempotent; never narrows an existing write
   * capability.
   */
  public CompletableFuture<Doc> importReadNamespaceAsync(String namespaceId) {
    return DataPlane.docsImportReadNamespace(runtime, nodeHandle, namespaceId)
        .thenApply(this::opened);
  }
}
