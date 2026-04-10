package com.aster.handle;

import com.aster.config.ConnectionConfig;
import com.aster.ffi.*;
import java.lang.foreign.*;
import java.util.HexFormat;
import java.util.concurrent.*;

public class IrohEndpoint extends IrohHandle {

  private final IrohRuntime runtime;

  IrohEndpoint(IrohRuntime runtime, long handle) {
    super(handle);
    this.runtime = runtime;
  }

  @Override
  protected String freeNativeKind() {
    return "iroh_endpoint";
  }

  /** Cleaner cannot call async FFI. This is a no-op; explicit {@link #close()} must be called. */
  @Override
  protected void freeNative(long handle) {
    // Cannot call async iroh_endpoint_close from Cleaner.
    // Explicit close() is required.
    System.err.println("IrohEndpoint: close() not called, handle " + handle + " leaked");
  }

  public IrohRuntime runtime() {
    return runtime;
  }

  /**
   * Get this endpoint's node ID as a hex string.
   *
   * @return the node ID string
   * @throws IrohException if the ID cannot be retrieved
   */
  public String nodeId() {
    var lib = IrohLibrary.getInstance();
    var alloc = lib.allocator();

    // Node IDs are typically 32 bytes (256 bits). Reserve extra capacity for hex encoding.
    var bufSeg = alloc.allocate(64);
    var lenSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    int status = lib.endpointId(runtime.nativeHandle(), nativeHandle(), bufSeg, 64, lenSeg);
    if (status != 0) {
      throw new IrohException(IrohStatus.fromCode(status), "iroh_endpoint_id failed: " + status);
    }

    int len = (int) lenSeg.get(ValueLayout.JAVA_LONG, 0);
    if (len == 0) {
      return "";
    }

    byte[] bytes = bufSeg.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
    // Hex-encode: node IDs are displayed as hex strings
    return HexFormat.of().formatHex(bytes);
  }

  /**
   * Connect to a remote node by node_id (hex string) and ALPN.
   *
   * @param nodeIdHex the remote node's ID as a hex string
   * @param alpn the ALPN protocol string
   * @return a future that completes with a connected IrohConnection
   */
  public CompletableFuture<IrohConnection> connectAsync(String nodeIdHex, String alpn) {
    var lib = IrohLibrary.getInstance();
    var alloc = lib.allocator();

    // Build iroh_connect_config_t via ConnectionConfig builder
    var configSeg = new ConnectionConfig().nodeId(nodeIdHex).alpn(alpn).toNative(alloc);

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    var connect =
        lib.getHandle(
            "iroh_connect",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS));

    try {
      int status =
          (int) connect.invoke(runtime.nativeHandle(), nativeHandle(), configSeg, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_connect failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_connect threw: " + t.getMessage());
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.CONNECTED) {
                return new IrohConnection(runtime, event.handle());
              }
              throw new IrohException("connect failed: unexpected event " + event.kind());
            });
  }

  /**
   * Accept an incoming connection on this endpoint.
   *
   * @return a future that completes with an accepted IrohConnection
   */
  public CompletableFuture<IrohConnection> acceptAsync() {
    var lib = IrohLibrary.getInstance();
    var alloc = lib.allocator();
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    var accept =
        lib.getHandle(
            "iroh_accept",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS));

    try {
      int status = (int) accept.invoke(nativeHandle(), 0L, opSeg);
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "iroh_accept failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_accept threw: " + t.getMessage());
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.CONNECTION_ACCEPTED) {
                return new IrohConnection(runtime, event.handle());
              }
              throw new IrohException("accept failed: unexpected event " + event.kind());
            });
  }

  /**
   * Asynchronously close this endpoint.
   *
   * @return a future that completes when the endpoint is closed
   */
  public CompletableFuture<Void> closeAsync() {
    var lib = IrohLibrary.getInstance();
    var alloc = lib.allocator();
    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    var endpointClose =
        lib.getHandle(
            "iroh_endpoint_close",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS));

    try {
      int status = (int) endpointClose.invoke(nativeHandle(), 0L, 0L, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_endpoint_close failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_endpoint_close threw: " + t.getMessage());
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime.registry().register(opId).thenApply(e -> null);
  }

  /** Synchronously close this endpoint. Blocks until closed. */
  @Override
  public void close() {
    try {
      closeAsync().get();
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
      throw new IrohException("close interrupted");
    } catch (ExecutionException e) {
      throw new IrohException("close failed: " + e.getCause().getMessage());
    }
  }
}
