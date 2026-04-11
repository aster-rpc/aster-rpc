package com.aster.handle;

import com.aster.ffi.*;
import java.lang.foreign.*;
import java.util.HexFormat;
import java.util.concurrent.*;

public class IrohConnection extends IrohHandle {

  private final IrohRuntime runtime;

  public IrohConnection(IrohRuntime runtime, long handle) {
    super(handle);
    this.runtime = runtime;
  }

  @Override
  protected String freeNativeKind() {
    return "iroh_connection";
  }

  @Override
  protected void freeNative(long handle) {
    // iroh_connection_close is sync: (runtime, connection, error_code, reason)
    // No async operation — safe to call from Cleaner.
    var lib = IrohLibrary.getInstance();
    var alloc = lib.allocator();

    var close =
        lib.getHandle(
            "iroh_connection_close",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG, // runtime
                ValueLayout.JAVA_LONG, // connection
                ValueLayout.JAVA_INT, // error_code
                IrohLibrary.IROH_BYTES // reason
                ));

    var emptyReason = alloc.allocate(IrohLibrary.IROH_BYTES);
    try {
      close.invoke(runtime.nativeHandle(), handle, 0, emptyReason);
    } catch (Throwable t) {
      System.err.println("iroh_connection_close failed: " + t.getMessage());
    }
  }

  public IrohRuntime runtime() {
    return runtime;
  }

  /**
   * Open a bidirectional stream on this connection.
   *
   * @return a future that completes with a bidirectional IrohStream
   */
  public CompletableFuture<IrohStream> openBiAsync() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    var openBi =
        lib.getHandle(
            "iroh_open_bi",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS));

    try {
      // runtime, connection, user_data, out_operation
      int status = (int) openBi.invoke(runtime.nativeHandle(), nativeHandle(), 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_open_bi failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_open_bi threw: " + t.getMessage());
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.STREAM_OPENED) {
                // handle = send_stream, related = recv_stream
                return new IrohStream(runtime, event.handle(), event.related());
              }
              throw new IrohException("open_bi failed: unexpected event " + event.kind());
            });
  }

  /**
   * Accept a bidirectional stream on this connection.
   *
   * @return a future that completes with an accepted IrohStream
   */
  public CompletableFuture<IrohStream> acceptBiAsync() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    var acceptBi =
        lib.getHandle(
            "iroh_accept_bi",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS));

    try {
      // runtime, connection, user_data, out_operation
      int status = (int) acceptBi.invoke(runtime.nativeHandle(), nativeHandle(), 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_accept_bi failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_accept_bi threw: " + t.getMessage());
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.STREAM_ACCEPTED) {
                return new IrohStream(runtime, event.handle(), event.related());
              }
              throw new IrohException("accept_bi failed: unexpected event " + event.kind());
            });
  }

  /**
   * Get the remote peer's node ID as a hex string.
   *
   * @return the remote node ID as a hex string
   */
  public String remoteId() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    // Node IDs are 32 bytes. Reserve extra capacity for hex encoding.
    var bufSeg = alloc.allocate(64);
    var lenSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.connectionRemoteId(runtime.nativeHandle(), nativeHandle(), bufSeg, 64, lenSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_connection_remote_id failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_connection_remote_id threw: " + t.getMessage());
    }

    int len = (int) lenSeg.get(ValueLayout.JAVA_LONG, 0);
    if (len == 0) {
      return "";
    }

    byte[] bytes = bufSeg.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
    return HexFormat.of().formatHex(bytes);
  }

  /**
   * Send a datagram on this connection.
   *
   * @param data the datagram payload
   * @return a future that completes when the datagram has been sent
   */
  public CompletableFuture<Void> sendDatagramAsync(byte[] data) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    // Build iroh_bytes_t for the data
    MemorySegment dataSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    MemorySegment heapSeg = alloc.allocate(data.length);
    heapSeg.copyFrom(MemorySegment.ofArray(data));
    dataSeg.set(ValueLayout.ADDRESS, 0, heapSeg);
    dataSeg.set(ValueLayout.JAVA_LONG, 8, (long) data.length);

    try {
      int status = lib.connectionSendDatagram(runtime.nativeHandle(), nativeHandle(), dataSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_connection_send_datagram failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_connection_send_datagram threw: " + t.getMessage()));
    }

    return CompletableFuture.completedFuture(null);
  }

  /**
   * Read an incoming datagram on this connection.
   *
   * @return a future that completes with the received datagram
   */
  public CompletableFuture<Datagram> readDatagramAsync() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.connectionReadDatagram(runtime.nativeHandle(), nativeHandle(), 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_connection_read_datagram failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_connection_read_datagram threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.BYTES_RESULT) {
                byte[] data = null;
                if (event.hasBuffer()
                    && event.data() != MemorySegment.NULL
                    && event.dataLen() > 0) {
                  data = event.data().asSlice(0, event.dataLen()).toArray(ValueLayout.JAVA_BYTE);
                  runtime.releaseBuffer(event.buffer());
                }
                return new Datagram(data);
              }
              throw new IrohException("readDatagram failed: unexpected event " + event.kind());
            });
  }

  /**
   * Get a future that completes when this connection is closed.
   *
   * @return a future that completes when the connection is closed
   */
  public CompletableFuture<Void> onClosedAsync() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status = lib.connectionClosed(runtime.nativeHandle(), nativeHandle(), 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_connection_closed failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_connection_closed threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime.registry().register(opId).thenApply(event -> null);
  }

  /**
   * Get the maximum datagram size for this connection.
   *
   * @return the maximum datagram size in bytes, or empty if datagrams are not supported
   */
  public java.util.OptionalInt maxDatagramSize() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    var sizeSeg = alloc.allocate(ValueLayout.JAVA_LONG);
    var isSomeSeg = alloc.allocate(ValueLayout.JAVA_INT);

    try {
      int status =
          lib.connectionMaxDatagramSize(runtime.nativeHandle(), nativeHandle(), sizeSeg, isSomeSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_connection_max_datagram_size failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_connection_max_datagram_size threw: " + t.getMessage());
    }

    int isSome = isSomeSeg.get(ValueLayout.JAVA_INT, 0);
    if (isSome == 0) {
      return java.util.OptionalInt.empty();
    }

    long size = sizeSeg.get(ValueLayout.JAVA_LONG, 0);
    return java.util.OptionalInt.of((int) size);
  }

  /**
   * Get the available datagram send buffer space.
   *
   * @return the available buffer space in bytes
   */
  public int datagramBufferSpace() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    var bytesSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.connectionDatagramSendBufferSpace(runtime.nativeHandle(), nativeHandle(), bytesSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status),
            "iroh_connection_datagram_send_buffer_space failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException(
          "iroh_connection_datagram_send_buffer_space threw: " + t.getMessage());
    }

    long bytes = bytesSeg.get(ValueLayout.JAVA_LONG, 0);
    return (int) bytes;
  }
}
