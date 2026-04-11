package com.aster.handle;

import com.aster.config.ConnectionConfig;
import com.aster.ffi.*;
import com.aster.node.NodeAddr;
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
    Arena confined = Arena.ofConfined();
    var alloc = confined;

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
   * Get this endpoint's structured address info including direct addresses.
   *
   * @return the endpoint's NodeAddr with direct addresses for peer connections
   * @throws IrohException if the address info cannot be retrieved
   */
  public NodeAddr addrInfo() {
    var lib = IrohLibrary.getInstance();
    // Use confined arena - kept alive for duration of this method
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    MemorySegment bufSeg = alloc.allocate(ValueLayout.JAVA_BYTE, 4096);
    // IROH_NODE_ADDR is 48 bytes but allocate(layout) creates 0-size segment
    // Use explicit size to ensure proper allocation
    MemorySegment addrSeg = alloc.allocate(48);

    int status =
        lib.endpointAddrInfo(runtime.nativeHandle(), nativeHandle(), bufSeg, 4096, addrSeg);
    if (status != 0) {
      throw new IrohException(
          IrohStatus.fromCode(status), "iroh_endpoint_addr_info failed: " + status);
    }

    String endpointId = readIrohBytes(addrSeg.asSlice(0, 16));
    String relayUrl = readIrohBytesOpt(addrSeg.asSlice(16, 16));
    java.util.List<String> directAddresses = readIrohBytesList(addrSeg.asSlice(32, 16));

    return new NodeAddr(endpointId, relayUrl, directAddresses);
  }

  private String readIrohBytes(MemorySegment seg) {
    // Use unaligned layouts since the data may not be 8-byte aligned
    MemorySegment ptr = seg.get(ValueLayout.ADDRESS_UNALIGNED, 0);
    long len = seg.get(ValueLayout.JAVA_LONG_UNALIGNED, 8);
    if (ptr != MemorySegment.NULL && len > 0) {
      return java.nio.charset.StandardCharsets.UTF_8
          .decode(ptr.reinterpret(len).asByteBuffer())
          .toString();
    }
    return "";
  }

  private String readIrohBytesOpt(MemorySegment seg) {
    // Use unaligned layouts since the data may not be 8-byte aligned
    MemorySegment ptr = seg.get(ValueLayout.ADDRESS_UNALIGNED, 0);
    long len = seg.get(ValueLayout.JAVA_LONG_UNALIGNED, 8);
    if (ptr != MemorySegment.NULL && len > 0) {
      return java.nio.charset.StandardCharsets.UTF_8
          .decode(ptr.reinterpret(len).asByteBuffer())
          .toString();
    }
    return null;
  }

  private java.util.List<String> readIrohBytesList(MemorySegment seg) {
    MemorySegment itemsPtr = seg.get(ValueLayout.ADDRESS, 0);
    long itemsLen = seg.get(ValueLayout.JAVA_LONG, 8);
    if (itemsPtr == MemorySegment.NULL || itemsLen == 0) {
      return java.util.Collections.emptyList();
    }

    java.util.List<String> result = new java.util.ArrayList<>();
    long itemSize = 16; // size of iroh_bytes_t { ptr: 8, len: 8 }
    for (int i = 0; i < itemsLen; i++) {
      // Create a properly sized segment using reinterpret after ofAddress
      MemorySegment itemSeg =
          MemorySegment.ofAddress(itemsPtr.address() + i * itemSize).reinterpret(16);
      // Use unaligned layouts since the data may not be 8-byte aligned
      MemorySegment itemPtr = itemSeg.get(ValueLayout.ADDRESS_UNALIGNED, 0);
      long itemLen = itemSeg.get(ValueLayout.JAVA_LONG_UNALIGNED, 8);
      if (itemPtr != MemorySegment.NULL && itemLen > 0) {
        result.add(
            java.nio.charset.StandardCharsets.UTF_8
                .decode(itemPtr.reinterpret(itemLen).asByteBuffer())
                .toString());
      }
    }
    return result;
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
    Arena confined = Arena.ofConfined();
    var alloc = confined;

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
   * Connect to a remote node using a structured node address and ALPN.
   *
   * @param addr the remote node's address
   * @param alpn the ALPN protocol string
   * @return a future that completes with a connected IrohConnection
   */
  public CompletableFuture<IrohConnection> connectNodeAddrAsync(NodeAddr addr, String alpn) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    // Build iroh_node_addr_t from the NodeAddr
    MemorySegment addrSeg = addr.toNative(alloc);

    // Build iroh_connect_config_t with the addr field set to point to addrSeg
    var configSeg =
        new ConnectionConfig()
            .nodeId(addr.endpointId())
            .alpn(alpn)
            .addr(addrSeg.address())
            .toNative(alloc);

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
    Arena confined = Arena.ofConfined();
    var alloc = confined;
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
      // runtime, endpoint, user_data, out_operation
      int status = (int) accept.invoke(runtime.nativeHandle(), nativeHandle(), 0L, opSeg);
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
    Arena confined = Arena.ofConfined();
    var alloc = confined;
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
      // runtime, endpoint, user_data, out_operation
      int status = (int) endpointClose.invoke(runtime.nativeHandle(), nativeHandle(), 0L, opSeg);
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

  /**
   * Export this endpoint's 32-byte secret key seed.
   *
   * @return the secret key bytes (32 bytes)
   * @throws IrohException if the secret key cannot be retrieved (not found)
   */
  public byte[] exportSecretKey() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    // Secret key is always 32 bytes
    var bufSeg = alloc.allocate(32);
    var lenSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    var exportSecretKey =
        lib.getHandle(
            "iroh_endpoint_export_secret_key",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS));

    try {
      int status =
          (int) exportSecretKey.invoke(runtime.nativeHandle(), nativeHandle(), bufSeg, 32L, lenSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_endpoint_export_secret_key failed: " + status);
      }
    } catch (Throwable t) {
      throw new IrohException("iroh_endpoint_export_secret_key threw: " + t.getMessage());
    }

    int len = (int) lenSeg.get(ValueLayout.JAVA_LONG, 0);
    if (len == 0) {
      return new byte[0];
    }

    return bufSeg.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
  }
}
