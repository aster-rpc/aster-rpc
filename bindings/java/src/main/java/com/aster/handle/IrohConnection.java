package com.aster.handle;

import com.aster.ffi.*;
import java.lang.foreign.*;
import java.util.concurrent.*;

public class IrohConnection extends IrohHandle {

  private final IrohRuntime runtime;

  IrohConnection(IrohRuntime runtime, long handle) {
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
    var alloc = lib.allocator();
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
      int status = (int) openBi.invoke(nativeHandle(), 0L, opSeg);
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
    var alloc = lib.allocator();
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
      int status = (int) acceptBi.invoke(nativeHandle(), 0L, opSeg);
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
}
