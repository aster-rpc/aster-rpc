package com.aster.docs;

import com.aster.ffi.IrohEventKind;
import com.aster.ffi.IrohException;
import com.aster.ffi.IrohLibrary;
import com.aster.ffi.IrohStatus;
import com.aster.handle.IrohRuntime;
import com.aster.node.IrohNode;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.CompletableFuture;

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
    MemorySegment dataSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
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
        throw new IrohException(IrohStatus.fromCode(status), "iroh_docs_create failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_docs_create threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.DOC_CREATED) {
                long docHandle = event.handle();
                return new Doc(runtime, docHandle);
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
            IrohStatus.fromCode(status), "iroh_docs_create_author failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_docs_create_author threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.AUTHOR_CREATED) {
                byte[] authorBytes = event.data().asByteBuffer().array();
                String authorHex = new String(authorBytes, StandardCharsets.UTF_8).trim();
                return AuthorId.of(authorHex.trim());
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

    var ticketBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    ticketBytesSeg.set(ValueLayout.ADDRESS, 0, ticketSeg);
    ticketBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) ticketBytes.length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.docsJoin(runtime.nativeHandle(), nodeHandle, ticketBytesSeg, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_docs_join failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_docs_join threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.DOC_JOINED) {
                long docHandle = event.handle();
                return new Doc(runtime, docHandle);
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

    var ticketBytesSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    ticketBytesSeg.set(ValueLayout.ADDRESS, 0, ticketSeg);
    ticketBytesSeg.set(ValueLayout.JAVA_LONG, 8, (long) ticketBytes.length);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.docsJoinAndSubscribe(runtime.nativeHandle(), nodeHandle, ticketBytesSeg, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_docs_join_and_subscribe failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_docs_join_and_subscribe threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.DOC_JOINED_AND_SUBSCRIBED) {
                long docHandle = event.handle();
                return new Doc(runtime, docHandle);
              }
              throw new IrohException("joinAndSubscribe failed: unexpected event " + event.kind());
            });
  }
}
