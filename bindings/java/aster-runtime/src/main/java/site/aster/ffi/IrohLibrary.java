package site.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.Optional;

/**
 * Loads the native iroh library and provides typed {@link MethodHandle} access to all FFI functions
 * defined in the C ABI.
 *
 * <p>Uses {@code SymbolLookup.libraryLookup} to load the native library and resolve symbols.
 */
public final class IrohLibrary implements SymbolLookup {

  private static volatile IrohLibrary INSTANCE;

  private static final String LIB_PATH =
      System.getenv("IROH_LIB_PATH") != null
          ? System.getenv("IROH_LIB_PATH")
          : "/Users/emrul/dev/emrul/iroh-python/target/release/libaster_transport_ffi.dylib";

  static {
    System.load(LIB_PATH);
  }

  private final SymbolLookup impl;
  private final Arena arena;
  private final SegmentAllocator allocator;
  private final MemorySegment runtimeHandleSegment;

  private IrohLibrary() {
    this.arena = Arena.ofAuto();
    this.impl = SymbolLookup.libraryLookup(LIB_PATH, arena);
    this.allocator = arena; // Arena implements SegmentAllocator
    this.runtimeHandleSegment = allocator.allocate(ValueLayout.JAVA_LONG);
  }

  public static IrohLibrary getInstance() {
    if (INSTANCE == null) {
      synchronized (IrohLibrary.class) {
        if (INSTANCE == null) {
          INSTANCE = new IrohLibrary();
        }
      }
    }
    return INSTANCE;
  }

  @Override
  public Optional<MemorySegment> find(String name) {
    return impl.find(name);
  }

  /** Returns a typed {@link MethodHandle} for the given FFI function. */
  public MethodHandle getHandle(String name, FunctionDescriptor desc) {
    MemorySegment symbol =
        impl.find(name)
            .orElseThrow(() -> new UnsatisfiedLinkError("iroh symbol not found: " + name));
    return Linker.nativeLinker().downcallHandle(symbol, desc);
  }

  // --- Version ---

  public int abiVersionMajor() {
    try {
      return (int)
          getHandle("iroh_abi_version_major", FunctionDescriptor.of(ValueLayout.JAVA_INT)).invoke();
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  public int abiVersionMinor() {
    try {
      return (int)
          getHandle("iroh_abi_version_minor", FunctionDescriptor.of(ValueLayout.JAVA_INT)).invoke();
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  public int abiVersionPatch() {
    try {
      return (int)
          getHandle("iroh_abi_version_patch", FunctionDescriptor.of(ValueLayout.JAVA_INT)).invoke();
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // --- Error ---

  /**
   * Get the last error message from the native layer.
   *
   * @param buffer the buffer to write the error message into
   * @param capacity the capacity of the buffer
   * @return the number of bytes written (excluding null terminator), or 0 if no error
   */
  public long lastErrorMessage(MemorySegment buffer, long capacity) {
    try {
      return (long)
          getHandle(
                  "iroh_last_error_message",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG))
              .invoke(buffer, capacity);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // --- Node identity ---

  /**
   * Get the node ID (bytes) for an endpoint.
   *
   * @param runtimeHandle the runtime handle
   * @param endpointHandle the endpoint handle
   * @param outBuf the output buffer to write the node ID into
   * @param capacity the capacity of the output buffer
   * @param outLen where to write the actual node ID length
   * @return status code (0 = OK)
   */
  public int endpointId(
      long runtimeHandle,
      long endpointHandle,
      MemorySegment outBuf,
      long capacity,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "iroh_endpoint_id",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, endpointHandle, outBuf, capacity, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  public SegmentAllocator allocator() {
    return allocator;
  }

  /** Arena shared across all FFI operations for reinterpreted native pointers. */
  public Arena sharedArena() {
    return arena;
  }

  /** Pre-allocated segment for runtime handle output (written by iroh_runtime_new). */
  public MemorySegment runtimeHandleSegment() {
    return runtimeHandleSegment;
  }

  // --- Node async FFI (iroh_node_*) ---

  /**
   * Create a memory-backed node asynchronously.
   *
   * @param runtimeHandle the runtime handle
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int nodeMemoryAsync(long runtimeHandle, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_node_memory",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Create a persistent node asynchronously.
   *
   * @param runtimeHandle the runtime handle
   * @param pathBytes the path bytes (valid for duration of call)
   * @param pathLen the length of the path in bytes (excluding null terminator)
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int nodePersistentAsync(
      long runtimeHandle, MemorySegment pathBytes, long pathLen, MemorySegment outOpId) {
    try {
      // Build iroh_bytes_t { ptr, len } struct on the stack/inline in the arena
      MemorySegment pathIrohBytes = allocator.allocate(IROH_BYTES); // 16 bytes
      pathIrohBytes.set(ValueLayout.ADDRESS, 0, pathBytes);
      pathIrohBytes.set(ValueLayout.JAVA_LONG, 8, pathLen);
      return (int)
          getHandle(
                  "iroh_node_persistent",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      IROH_BYTES, // path: iroh_bytes_t { ptr, len }
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, pathIrohBytes, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Create a memory-backed node with custom ALPNs asynchronously.
   *
   * @param runtimeHandle the runtime handle
   * @param alpnItemsPtr pointer to array of alpn byte arrays
   * @param alpnLensPtr pointer to array of alpn lengths
   * @param alpnCount number of ALPNs
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int nodeMemoryWithAlpnsAsync(
      long runtimeHandle,
      MemorySegment alpnItemsPtr,
      MemorySegment alpnLensPtr,
      long alpnCount,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_node_memory_with_alpns",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, alpnItemsPtr, alpnLensPtr, alpnCount, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Accept an incoming aster connection on a node.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int nodeAcceptAsterAsync(long runtimeHandle, long nodeHandle, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_node_accept_aster",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Long-poll the reactor for the next non-rpc aster-ALPN connection. Emits {@code
   * IROH_EVENT_ASTER_ACCEPTED} on arrival: event.handle = connection handle, event.data = ALPN
   * bytes. See {@code aster_reactor_accept_non_rpc} in the C header.
   */
  public int asterReactorAcceptNonRpc(
      long runtimeHandle, long reactorHandle, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "aster_reactor_accept_non_rpc",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG, // runtime
                      ValueLayout.JAVA_LONG, // reactor
                      ValueLayout.JAVA_LONG, // user_data
                      ValueLayout.ADDRESS)) // out_operation
              .invoke(runtimeHandle, reactorHandle, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Close a node asynchronously.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int nodeCloseAsync(long runtimeHandle, long nodeHandle, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_node_close",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Get the node ID (bytes) for a node.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param outBuf output buffer
   * @param capacity buffer capacity
   * @param outLen where to write the actual length
   * @return status code (0 = OK)
   */
  public int nodeId(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment outBuf,
      long capacity,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "iroh_node_id",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, outBuf, capacity, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Export the secret key for a node.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param outBuf output buffer
   * @param capacity buffer capacity
   * @param outLen where to write the actual length
   * @return status code (0 = OK)
   */
  public int nodeExportSecretKey(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment outBuf,
      long capacity,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "iroh_node_export_secret_key",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, outBuf, capacity, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Get structured address info for a node.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param outBuf output buffer for string data
   * @param bufCapacity buffer capacity
   * @param outAddrSegment output segment for the iroh_node_addr_t struct
   * @return status code (0 = OK)
   */
  public int nodeAddrInfo(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment outBuf,
      long bufCapacity,
      MemorySegment outAddrSegment) {
    try {
      return (int)
          getHandle(
                  "iroh_node_addr_info",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, outBuf, bufCapacity, outAddrSegment);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Get the bound address info for an endpoint.
   *
   * @param runtimeHandle the runtime handle
   * @param endpointHandle the endpoint handle
   * @param outBuf output buffer for string data (relay URL, direct addresses)
   * @param bufCapacity buffer capacity
   * @param outAddrSegment output segment for the iroh_node_addr_t struct
   * @return status code (0 = OK)
   */
  public int endpointAddrInfo(
      long runtimeHandle,
      long endpointHandle,
      MemorySegment outBuf,
      long bufCapacity,
      MemorySegment outAddrSegment) {
    try {
      return (int)
          getHandle(
                  "iroh_endpoint_addr_info",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, endpointHandle, outBuf, bufCapacity, outAddrSegment);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Free a node handle synchronously.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @return status code (0 = OK)
   */
  public int nodeFree(long runtimeHandle, long nodeHandle) {
    try {
      return (int)
          getHandle(
                  "iroh_node_free",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG))
              .invoke(runtimeHandle, nodeHandle);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Add a node address to an endpoint.
   *
   * @param runtimeHandle the runtime handle
   * @param endpointHandle the endpoint handle
   * @param addrSegment segment containing the iroh_node_addr_t struct
   * @return status code (0 = OK)
   */
  public int addNodeAddr(long runtimeHandle, long endpointHandle, MemorySegment addrSegment) {
    try {
      return (int)
          getHandle(
                  "iroh_add_node_addr",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, endpointHandle, addrSegment);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // --- Connection extras ---

  /**
   * Get the remote peer's node ID.
   *
   * @param runtimeHandle the runtime handle
   * @param connectionHandle the connection handle
   * @param outBuf output buffer for the node ID bytes
   * @param capacity buffer capacity
   * @param outLen where to write the actual length
   * @return status code (0 = OK)
   */
  public int connectionRemoteId(
      long runtimeHandle,
      long connectionHandle,
      MemorySegment outBuf,
      long capacity,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "iroh_connection_remote_id",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, connectionHandle, outBuf, capacity, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Send a datagram on a connection.
   *
   * @param runtimeHandle the runtime handle
   * @param connectionHandle the connection handle
   * @param dataSegment segment containing the datagram data (iroh_bytes_t)
   * @return status code (0 = OK)
   */
  public int connectionSendDatagram(
      long runtimeHandle, long connectionHandle, MemorySegment dataSegment) {
    try {
      return (int)
          getHandle(
                  "iroh_connection_send_datagram",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, connectionHandle, dataSegment);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Read a datagram from a connection.
   *
   * @param runtimeHandle the runtime handle
   * @param connectionHandle the connection handle
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int connectionReadDatagram(
      long runtimeHandle, long connectionHandle, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_connection_read_datagram",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, connectionHandle, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Wait for a connection to be closed.
   *
   * @param runtimeHandle the runtime handle
   * @param connectionHandle the connection handle
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int connectionClosed(
      long runtimeHandle, long connectionHandle, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_connection_closed",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, connectionHandle, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Get the max datagram size for a connection.
   *
   * @param runtimeHandle the runtime handle
   * @param connectionHandle the connection handle
   * @param outSize where to write the max size
   * @param outIsSome where to write whether the size is known (1=true, 0=false)
   * @return status code (0 = OK)
   */
  public int connectionMaxDatagramSize(
      long runtimeHandle, long connectionHandle, MemorySegment outSize, MemorySegment outIsSome) {
    try {
      return (int)
          getHandle(
                  "iroh_connection_max_datagram_size",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, connectionHandle, outSize, outIsSome);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Get the available datagram send buffer space.
   *
   * @param runtimeHandle the runtime handle
   * @param connectionHandle the connection handle
   * @param outBytes where to write the available bytes
   * @return status code (0 = OK)
   */
  public int connectionDatagramSendBufferSpace(
      long runtimeHandle, long connectionHandle, MemorySegment outBytes) {
    try {
      return (int)
          getHandle(
                  "iroh_connection_datagram_send_buffer_space",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, connectionHandle, outBytes);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // --- Blobs ---

  /**
   * Add bytes to the blob store.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param dataSegment segment containing the data (iroh_bytes_t)
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int blobsAddBytes(
      long runtimeHandle, long nodeHandle, MemorySegment dataSegment, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_add_bytes",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      IROH_BYTES,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, dataSegment, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Read blob data.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param hashHexSegment segment containing the hash hex string (iroh_bytes_t)
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int blobsRead(
      long runtimeHandle, long nodeHandle, MemorySegment hashHexSegment, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_read",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, hashHexSegment, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Add bytes as a named collection entry.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param nameSegment segment containing the name (iroh_bytes_t)
   * @param dataSegment segment containing the data (iroh_bytes_t)
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int blobsAddBytesAsCollection(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment nameSegment,
      MemorySegment dataSegment,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_add_bytes_as_collection",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, nameSegment, dataSegment, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Add a multi-file collection.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param entriesJsonSegment segment containing the JSON string (iroh_bytes_t)
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int blobsAddCollection(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment entriesJsonSegment,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_add_collection",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, entriesJsonSegment, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * List collection entries.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param hashHexSegment segment containing the collection hash (iroh_bytes_t)
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int blobsListCollection(
      long runtimeHandle, long nodeHandle, MemorySegment hashHexSegment, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_list_collection",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, hashHexSegment, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Create a ticket for a blob.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param hashHexSegment segment containing the blob hash (iroh_bytes_t)
   * @param outBuf output buffer for the ticket string
   * @param capacity buffer capacity
   * @param outLen where to write the actual length
   * @return status code (0 = OK)
   */
  public int blobsCreateTicket(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment hashHexSegment,
      MemorySegment outBuf,
      long capacity,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_create_ticket",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, hashHexSegment, outBuf, capacity, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Create a ticket for a collection.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param hashHexSegment segment containing the collection hash (iroh_bytes_t)
   * @param outBuf output buffer for the ticket string
   * @param capacity buffer capacity
   * @param outLen where to write the actual length
   * @return status code (0 = OK)
   */
  public int blobsCreateCollectionTicket(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment hashHexSegment,
      MemorySegment outBuf,
      long capacity,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_create_collection_ticket",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, hashHexSegment, outBuf, capacity, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Download a blob from a ticket.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param ticketSegment segment containing the ticket (iroh_bytes_t)
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int blobsDownload(
      long runtimeHandle, long nodeHandle, MemorySegment ticketSegment, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_download",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, ticketSegment, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Get blob status synchronously.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param hashHexPtr pointer to the hash hex string
   * @param hashHexLen length of the hash hex string
   * @param outStatus where to write the status (0=not_found, 1=partial, 2=complete)
   * @param outSize where to write the size in bytes
   * @return status code (0 = OK)
   */
  public int blobsStatus(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment hashHexPtr,
      long hashHexLen,
      MemorySegment outStatus,
      MemorySegment outSize) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_status",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, hashHexPtr, hashHexLen, outStatus, outSize);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Check if blob is stored locally.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param hashHexPtr pointer to the hash hex string
   * @param hashHexLen length of the hash hex string
   * @param outHas where to write whether blob is complete (1=yes, 0=no)
   * @return status code (0 = OK)
   */
  public int blobsHas(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment hashHexPtr,
      long hashHexLen,
      MemorySegment outHas) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_has",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, hashHexPtr, hashHexLen, outHas);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Observe blob download snapshot.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param hashHexPtr pointer to the hash hex string
   * @param hashHexLen length of the hash hex string
   * @param outIsComplete where to write whether blob is complete
   * @param outSize where to write the total size
   * @return status code (0 = OK)
   */
  public int blobsObserveSnapshot(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment hashHexPtr,
      long hashHexLen,
      MemorySegment outIsComplete,
      MemorySegment outSize) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_observe_snapshot",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, hashHexPtr, hashHexLen, outIsComplete, outSize);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Observe blob download completion.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param hashHexPtr pointer to the hash hex string
   * @param hashHexLen length of the hash hex string
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int blobsObserveComplete(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment hashHexPtr,
      long hashHexLen,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_observe_complete",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, hashHexPtr, hashHexLen, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Get local blob info.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param hashHexPtr pointer to the hash hex string
   * @param hashHexLen length of the hash hex string
   * @param outIsComplete where to write whether blob is complete locally
   * @param outLocalBytes where to write the local byte count
   * @return status code (0 = OK)
   */
  public int blobsLocalInfo(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment hashHexPtr,
      long hashHexLen,
      MemorySegment outIsComplete,
      MemorySegment outLocalBytes) {
    try {
      return (int)
          getHandle(
                  "iroh_blobs_local_info",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(
                  runtimeHandle, nodeHandle, hashHexPtr, hashHexLen, outIsComplete, outLocalBytes);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // ============================================================================
  // Gossip
  // ============================================================================

  /**
   * Subscribe to a gossip topic.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param topicSegment iroh_bytes_t topic (struct by value)
   * @param peersSegment iroh_bytes_list_t peers (struct by value)
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int gossipSubscribe(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment topicSegment,
      MemorySegment peersSegment,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_gossip_subscribe",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      IROH_BYTES,
                      IROH_BYTES_LIST,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, topicSegment, peersSegment, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Broadcast data to a gossip topic.
   *
   * @param runtimeHandle the runtime handle
   * @param topicHandle the gossip topic handle
   * @param dataSegment iroh_bytes_t data (struct by value)
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int gossipBroadcast(
      long runtimeHandle, long topicHandle, MemorySegment dataSegment, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_gossip_broadcast",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      IROH_BYTES,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, topicHandle, dataSegment, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Receive the next message from a gossip topic.
   *
   * @param runtimeHandle the runtime handle
   * @param topicHandle the gossip topic handle
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int gossipRecv(long runtimeHandle, long topicHandle, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_gossip_recv",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, topicHandle, 0L, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Free a gossip topic handle.
   *
   * @param runtimeHandle the runtime handle
   * @param topicHandle the gossip topic handle
   * @return status code (0 = OK)
   */
  public int gossipTopicFree(long runtimeHandle, long topicHandle) {
    try {
      return (int)
          getHandle(
                  "iroh_gossip_topic_free",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG))
              .invoke(runtimeHandle, topicHandle);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // ============================================================================
  // Tags
  // ============================================================================

  /**
   * Set a named tag. format: 0 = raw, 1 = hash_seq. Emits IROH_EVENT_TAG_SET.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param namePtr pointer to the tag name string
   * @param nameLen length of the tag name
   * @param hashHexPtr pointer to the hash hex string
   * @param hashHexLen length of the hash hex string
   * @param format the blob format (0=raw, 1=hash_seq)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int tagsSet(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment namePtr,
      long nameLen,
      MemorySegment hashHexPtr,
      long hashHexLen,
      int format,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_tags_set",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(
                  runtimeHandle,
                  nodeHandle,
                  namePtr,
                  nameLen,
                  hashHexPtr,
                  hashHexLen,
                  format,
                  userData,
                  outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Get a tag by name. Emits IROH_EVENT_TAG_GET with payload on found, NOT_FOUND status if absent.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param namePtr pointer to the tag name string
   * @param nameLen length of the tag name
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int tagsGet(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment namePtr,
      long nameLen,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_tags_get",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, namePtr, nameLen, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Delete a tag by name. Emits IROH_EVENT_TAG_DELETED with count in event.flags.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param namePtr pointer to the tag name string
   * @param nameLen length of the tag name
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int tagsDelete(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment namePtr,
      long nameLen,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_tags_delete",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, namePtr, nameLen, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * List tags matching a prefix (empty prefix = all tags). Emits IROH_EVENT_TAG_LIST with packed
   * tag records in payload; event.flags = count.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param prefixPtr pointer to the prefix string
   * @param prefixLen length of the prefix (0 for all tags)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int tagsListPrefix(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment prefixPtr,
      long prefixLen,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_tags_list_prefix",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, prefixPtr, prefixLen, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // ============================================================================
  // Docs
  // ============================================================================

  /**
   * Create a new document.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docsCreate(long runtimeHandle, long nodeHandle, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_docs_create",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Create a new author for content addressing.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docsCreateAuthor(
      long runtimeHandle, long nodeHandle, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_docs_create_author",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Join a document from a ticket.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param ticketSegment segment containing the ticket (iroh_bytes_t)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docsJoin(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment ticketSegment,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_docs_join",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, ticketSegment, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Set bytes in a document.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle
   * @param authorSegment segment containing the author hex (iroh_bytes_t)
   * @param keySegment segment containing the key (iroh_bytes_t)
   * @param valueSegment segment containing the value (iroh_bytes_t)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docSetBytes(
      long runtimeHandle,
      long docHandle,
      MemorySegment authorSegment,
      MemorySegment keySegment,
      MemorySegment valueSegment,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_set_bytes",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(
                  runtimeHandle,
                  docHandle,
                  authorSegment,
                  keySegment,
                  valueSegment,
                  userData,
                  outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Get exact entry from a document.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle
   * @param authorSegment segment containing the author hex (iroh_bytes_t)
   * @param keySegment segment containing the key (iroh_bytes_t)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docGetExact(
      long runtimeHandle,
      long docHandle,
      MemorySegment authorSegment,
      MemorySegment keySegment,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_get_exact",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, authorSegment, keySegment, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Share a document.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle
   * @param mode the share mode (0=read, 1=write)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docShare(
      long runtimeHandle, long docHandle, int mode, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_share",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, mode, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Query a document.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle
   * @param mode the query mode (0=author, 1=all, 2=prefix)
   * @param keySegment segment containing the key prefix (iroh_bytes_t)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docQuery(
      long runtimeHandle,
      long docHandle,
      int mode,
      MemorySegment keySegment,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_query",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_INT,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, mode, keySegment, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Read entry content from a document.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle
   * @param contentHashSegment segment containing the content hash hex (iroh_bytes_t)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docReadEntryContent(
      long runtimeHandle,
      long docHandle,
      MemorySegment contentHashSegment,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_read_entry_content",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, contentHashSegment, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Start syncing a document.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle
   * @param peersSegment segment containing the peers list (iroh_bytes_list_t)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docStartSync(
      long runtimeHandle,
      long docHandle,
      MemorySegment peersSegment,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_start_sync",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, peersSegment, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Leave (stop syncing) a document.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docLeave(long runtimeHandle, long docHandle, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_leave",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Subscribe to document events.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docSubscribe(
      long runtimeHandle, long docHandle, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_subscribe",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Receive document events.
   *
   * @param runtimeHandle the runtime handle
   * @param receiverHandle the receiver handle from subscribe
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docEventRecv(
      long runtimeHandle, long receiverHandle, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_event_recv",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, receiverHandle, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Set download policy for a document.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle
   * @param mode the download policy mode
   * @param prefixesSegment segment containing the prefixes list (iroh_bytes_list_t)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docSetDownloadPolicy(
      long runtimeHandle,
      long docHandle,
      int mode,
      MemorySegment prefixesSegment,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_set_download_policy",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_INT,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, mode, prefixesSegment, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Share a document with address info.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle
   * @param mode the share mode (0=read, 1=write)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docShareWithAddr(
      long runtimeHandle, long docHandle, int mode, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_share_with_addr",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, mode, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Join and subscribe to a document atomically.
   *
   * @param runtimeHandle the runtime handle
   * @param nodeHandle the node handle
   * @param ticketSegment segment containing the ticket (iroh_bytes_t)
   * @param userData user data passed to the operation
   * @param outOpId where to write the operation id
   * @return status code (0 = OK)
   */
  public int docsJoinAndSubscribe(
      long runtimeHandle,
      long nodeHandle,
      MemorySegment ticketSegment,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "iroh_docs_join_and_subscribe",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, nodeHandle, ticketSegment, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Free a document handle.
   *
   * @param runtimeHandle the runtime handle
   * @param docHandle the document handle to free
   * @return status code (0 = OK)
   */
  public int docFree(long runtimeHandle, long docHandle) {
    try {
      return (int)
          getHandle(
                  "iroh_doc_free",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG))
              .invoke(runtimeHandle, docHandle);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // --- Struct layouts (verified against Rust #[repr(C)] structs in ffi/src/lib.rs) ---

  public static final MemoryLayout IROH_BYTES =
      MemoryLayout.structLayout(
          ValueLayout.ADDRESS.withName("ptr"), // 0
          ValueLayout.JAVA_LONG.withName("len") // 8
          );

  public static final MemoryLayout IROH_BYTES_LIST =
      MemoryLayout.structLayout(
          ValueLayout.ADDRESS.withName("items"), // 0
          ValueLayout.JAVA_LONG.withName("len") // 8
          );

  /**
   * iroh_runtime_config_t: struct_size, worker_threads, event_queue_capacity, reserved. 16 bytes.
   */
  public static final MemoryLayout IROH_RUNTIME_CONFIG =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("worker_threads"), // 4
          ValueLayout.JAVA_INT.withName("event_queue_capacity"), // 8
          ValueLayout.JAVA_INT.withName("reserved") // 12
          );

  /**
   * iroh_endpoint_config_t: struct_size, relay_mode, secret_key, alpns, relay_urls,
   * enable_discovery, enable_hooks, hook_timeout_ms, bind_addr, clear_ip_transports,
   * clear_relay_transports, portmapper_config, proxy_url, proxy_from_env, data_dir_utf8.
   *
   * <p>Total: 144 bytes. Matches Rust iroh_endpoint_config_t exactly.
   */
  public static final MemoryLayout IROH_ENDPOINT_CONFIG =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("relay_mode"), // 4
          IROH_BYTES.withName("secret_key"), // 8  (ptr+len = 16 bytes)
          IROH_BYTES_LIST.withName("alpns"), // 24 (items+len = 16 bytes)
          IROH_BYTES_LIST.withName("relay_urls"), // 40 (items+len = 16 bytes)
          ValueLayout.JAVA_INT.withName("enable_discovery"), // 56
          ValueLayout.JAVA_INT.withName("enable_hooks"), // 60
          ValueLayout.JAVA_LONG.withName("hook_timeout_ms"), // 64
          IROH_BYTES.withName("bind_addr"), // 72 (ptr+len = 16 bytes)
          ValueLayout.JAVA_INT.withName("clear_ip_transports"), // 88
          ValueLayout.JAVA_INT.withName("clear_relay_transports"), // 92
          ValueLayout.JAVA_INT.withName("portmapper_config"), // 96
          MemoryLayout.paddingLayout(4), // 4 bytes padding → proxy_url at 104 (8-byte aligned)
          IROH_BYTES.withName("proxy_url"), // 104 (ptr+len = 16 bytes)
          ValueLayout.JAVA_INT.withName("proxy_from_env"), // 120
          MemoryLayout.paddingLayout(4), // 4 bytes padding → data_dir_utf8 at 128 (8-byte aligned)
          IROH_BYTES.withName("data_dir_utf8") // 128 (ptr+len = 16 bytes)
          );

  /**
   * iroh_connect_config_t: struct_size, flags, node_id (hex string), alpn, addr. Total: 48 bytes.
   * addr is *const iroh_node_addr_t, passed as NULL for simple connect.
   */
  public static final MemoryLayout IROH_CONNECT_CONFIG =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("flags"), // 4
          IROH_BYTES.withName("node_id"), // 8  (ptr+len = 16 bytes)
          IROH_BYTES.withName("alpn"), // 24 (ptr+len = 16 bytes)
          ValueLayout.ADDRESS.withName("addr") // 40
          );

  /** iroh_node_addr_t: endpoint_id, relay_url, direct_addresses. Total: 48 bytes. */
  public static final MemoryLayout IROH_NODE_ADDR =
      MemoryLayout.structLayout(
          IROH_BYTES.withName("endpoint_id"), // 0  (ptr+len = 16 bytes)
          IROH_BYTES.withName("relay_url"), // 16 (ptr+len = 16 bytes)
          IROH_BYTES_LIST.withName("direct_addresses") // 32 (items+len = 16 bytes)
          );

  /**
   * iroh_event_t: matches Rust iroh_event_t exactly. Total: 80 bytes.
   *
   * <p>Note: 4 bytes of padding at offset 12 between status and operation (u64 alignment).
   */
  public static final MemoryLayout IROH_EVENT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("kind"), // 4
          ValueLayout.JAVA_INT.withName("status"), // 8
          MemoryLayout.paddingLayout(4), // 12 — alignment padding
          ValueLayout.JAVA_LONG.withName("operation"), // 16
          ValueLayout.JAVA_LONG.withName("handle"), // 24
          ValueLayout.JAVA_LONG.withName("related"), // 32
          ValueLayout.JAVA_LONG.withName("user_data"), // 40
          ValueLayout.ADDRESS.withName("data_ptr"), // 48
          ValueLayout.JAVA_LONG.withName("data_len"), // 56
          ValueLayout.JAVA_LONG.withName("buffer"), // 64
          ValueLayout.JAVA_INT.withName("error_code"), // 72
          ValueLayout.JAVA_INT.withName("flags") // 76
          );

  // --- Aster contract identity (aster_contract_id, aster_canonical_bytes, aster_blake3_hex) ---

  /**
   * Compute the contract_id (64-char hex BLAKE3) from a ServiceContract JSON.
   *
   * <p>C signature: {@code int32_t aster_contract_id(const uint8_t *json_ptr, uintptr_t json_len,
   * uint8_t *out_buf, uintptr_t *out_len);}
   *
   * @param jsonPtr pointer to UTF-8 JSON bytes
   * @param jsonLen length of JSON
   * @param outBuf caller-owned output buffer (at least 65 bytes for 64-char hex + null)
   * @param outLen pointer to uintptr_t: in = buffer capacity, out = bytes written
   * @return status code (0 = OK)
   */
  public int asterContractId(
      MemorySegment jsonPtr, long jsonLen, MemorySegment outBuf, MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "aster_contract_id",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(jsonPtr, jsonLen, outBuf, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Compute canonical bytes for a named type from JSON.
   *
   * <p>C signature: {@code int32_t aster_canonical_bytes(const uint8_t *type_name_ptr, uintptr_t
   * type_name_len, const uint8_t *json_ptr, uintptr_t json_len, uint8_t *out_buf, uintptr_t
   * *out_len);}
   *
   * @param typeNamePtr pointer to type name ("ServiceContract", "TypeDef", "MethodDef")
   * @param typeNameLen length of type name
   * @param jsonPtr pointer to UTF-8 JSON bytes
   * @param jsonLen length of JSON
   * @param outBuf caller-owned output buffer
   * @param outLen pointer to uintptr_t: in = capacity, out = bytes written
   * @return status code (0 = OK)
   */
  public int asterCanonicalBytes(
      MemorySegment typeNamePtr,
      long typeNameLen,
      MemorySegment jsonPtr,
      long jsonLen,
      MemorySegment outBuf,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "aster_canonical_bytes",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(typeNamePtr, typeNameLen, jsonPtr, jsonLen, outBuf, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * BLAKE3 hash of arbitrary bytes → 64-char lowercase hex. Keeps the "no local hashing in
   * bindings" rule (§11.3.2.3) intact: bindings compose this with {@link #asterCanonicalBytes} when
   * they need per-TypeDef hashes during contract_id derivation.
   *
   * <p>C signature: {@code int32_t aster_blake3_hex(const uint8_t *bytes_ptr, uintptr_t bytes_len,
   * uint8_t *out_buf, uintptr_t *out_len);}
   */
  public int asterBlake3Hex(
      MemorySegment bytesPtr, long bytesLen, MemorySegment outBuf, MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "aster_blake3_hex",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(bytesPtr, bytesLen, outBuf, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Decode an {@code aster1…} ticket string into its structured parts. The output buffer receives a
   * UTF-8 JSON object: {@code {endpoint_id, relay_addr, direct_addrs, credential_type,
   * credential_data_hex}}. See the Rust {@code aster_transport_core::ticket::AsterTicket} for field
   * semantics.
   *
   * <p>C signature: {@code int32_t aster_ticket_decode(const uint8_t *ticket_ptr, uintptr_t
   * ticket_len, uint8_t *out_buf, uintptr_t *out_len);}
   */
  public int asterTicketDecode(
      MemorySegment ticketPtr, long ticketLen, MemorySegment outBuf, MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "aster_ticket_decode",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(ticketPtr, ticketLen, outBuf, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * Encode a {@code (endpoint_id, relay_addr, direct_addrs, credential)} tuple into an {@code
   * aster1<base58>} ticket string. All string arguments are UTF-8 byte pointers; pass a null
   * pointer with length 0 to omit optional fields (relay, direct addrs, credential). Output buffer
   * receives the ticket string; {@code outLen} is in/out — set to buffer size on entry, actual
   * length on success.
   *
   * <p>C signature: {@code int32_t aster_ticket_encode(const uint8_t *endpoint_id_hex_ptr,
   * uintptr_t endpoint_id_hex_len, const uint8_t *relay_addr_ptr, uintptr_t relay_addr_len, const
   * uint8_t *direct_addrs_json_ptr, uintptr_t direct_addrs_json_len, const uint8_t
   * *credential_type_ptr, uintptr_t credential_type_len, const uint8_t *credential_data_ptr,
   * uintptr_t credential_data_len, uint8_t *out_buf, uintptr_t *out_len);}
   */
  public int asterTicketEncode(
      MemorySegment endpointIdHex,
      long endpointIdHexLen,
      MemorySegment relayAddr,
      long relayAddrLen,
      MemorySegment directAddrsJson,
      long directAddrsJsonLen,
      MemorySegment credentialType,
      long credentialTypeLen,
      MemorySegment credentialData,
      long credentialDataLen,
      MemorySegment outBuf,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "aster_ticket_encode",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(
                  endpointIdHex,
                  endpointIdHexLen,
                  relayAddr,
                  relayAddrLen,
                  directAddrsJson,
                  directAddrsJsonLen,
                  credentialType,
                  credentialTypeLen,
                  credentialData,
                  credentialDataLen,
                  outBuf,
                  outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // --- Registry FFI (§11) ------------------------------------------------

  /** C signature: {@code int64_t aster_registry_now_epoch_ms(void);} */
  public long asterRegistryNowEpochMs() {
    try {
      return (long)
          getHandle("aster_registry_now_epoch_ms", FunctionDescriptor.of(ValueLayout.JAVA_LONG))
              .invoke();
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * C signature: {@code int32_t aster_registry_is_fresh(const uint8_t *lease_json_ptr, uintptr_t
   * lease_json_len, int32_t lease_duration_s);}
   */
  public int asterRegistryIsFresh(
      MemorySegment leaseJsonPtr, long leaseJsonLen, int leaseDurationS) {
    try {
      return (int)
          getHandle(
                  "aster_registry_is_fresh",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_INT))
              .invoke(leaseJsonPtr, leaseJsonLen, leaseDurationS);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * C signature: {@code int32_t aster_registry_is_routable(const uint8_t *status_ptr, uintptr_t
   * status_len);}
   */
  public int asterRegistryIsRoutable(MemorySegment statusPtr, long statusLen) {
    try {
      return (int)
          getHandle(
                  "aster_registry_is_routable",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG))
              .invoke(statusPtr, statusLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * C signature: {@code int32_t aster_registry_filter_and_rank(const uint8_t *leases_json_ptr,
   * uintptr_t leases_json_len, const uint8_t *opts_json_ptr, uintptr_t opts_json_len, uint8_t
   * *out_buf, uintptr_t *out_len);}
   */
  public int asterRegistryFilterAndRank(
      MemorySegment leasesJsonPtr,
      long leasesJsonLen,
      MemorySegment optsJsonPtr,
      long optsJsonLen,
      MemorySegment outBuf,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "aster_registry_filter_and_rank",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(leasesJsonPtr, leasesJsonLen, optsJsonPtr, optsJsonLen, outBuf, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * C signature: {@code int32_t aster_registry_key(int32_t kind, const uint8_t *arg1_ptr, uintptr_t
   * arg1_len, const uint8_t *arg2_ptr, uintptr_t arg2_len, const uint8_t *arg3_ptr, uintptr_t
   * arg3_len, uint8_t *out_buf, uintptr_t *out_len);}
   */
  public int asterRegistryKey(
      int kind,
      MemorySegment arg1Ptr,
      long arg1Len,
      MemorySegment arg2Ptr,
      long arg2Len,
      MemorySegment arg3Ptr,
      long arg3Len,
      MemorySegment outBuf,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "aster_registry_key",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_INT,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.ADDRESS))
              .invoke(kind, arg1Ptr, arg1Len, arg2Ptr, arg2Len, arg3Ptr, arg3Len, outBuf, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // --- Registry async doc-backed ops (event kinds 80-84) ----------------

  /**
   * C signature: {@code int32_t aster_registry_resolve(iroh_runtime_t runtime, uint64_t doc, struct
   * iroh_bytes_t opts_json, uint64_t user_data, iroh_operation_t *out_operation);}
   */
  public int asterRegistryResolve(
      long runtimeHandle,
      long docHandle,
      MemorySegment optsJsonPtr,
      long optsJsonLen,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "aster_registry_resolve",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, optsJsonPtr, optsJsonLen, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * C signature: {@code int32_t aster_registry_publish(iroh_runtime_t runtime, uint64_t doc, struct
   * iroh_bytes_t author_id, struct iroh_bytes_t lease_json, struct iroh_bytes_t artifact_json,
   * struct iroh_bytes_t service, int32_t version, struct iroh_bytes_t channel, uint64_t
   * gossip_topic, uint64_t user_data, iroh_operation_t *out_operation);}
   */
  public int asterRegistryPublish(
      long runtimeHandle,
      long docHandle,
      MemorySegment authorIdPtr,
      long authorIdLen,
      MemorySegment leaseJsonPtr,
      long leaseJsonLen,
      MemorySegment artifactJsonPtr,
      long artifactJsonLen,
      MemorySegment servicePtr,
      long serviceLen,
      int version,
      MemorySegment channelPtr,
      long channelLen,
      long gossipTopic,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "aster_registry_publish",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG, // runtime
                      ValueLayout.JAVA_LONG, // doc
                      ValueLayout.ADDRESS, // author ptr
                      ValueLayout.JAVA_LONG, // author len
                      ValueLayout.ADDRESS, // lease ptr
                      ValueLayout.JAVA_LONG, // lease len
                      ValueLayout.ADDRESS, // artifact ptr
                      ValueLayout.JAVA_LONG, // artifact len
                      ValueLayout.ADDRESS, // service ptr
                      ValueLayout.JAVA_LONG, // service len
                      ValueLayout.JAVA_INT, // version
                      ValueLayout.ADDRESS, // channel ptr
                      ValueLayout.JAVA_LONG, // channel len
                      ValueLayout.JAVA_LONG, // gossip topic
                      ValueLayout.JAVA_LONG, // user data
                      ValueLayout.ADDRESS)) // out op
              .invoke(
                  runtimeHandle,
                  docHandle,
                  authorIdPtr,
                  authorIdLen,
                  leaseJsonPtr,
                  leaseJsonLen,
                  artifactJsonPtr,
                  artifactJsonLen,
                  servicePtr,
                  serviceLen,
                  version,
                  channelPtr,
                  channelLen,
                  gossipTopic,
                  userData,
                  outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * C signature: {@code int32_t aster_registry_renew_lease(iroh_runtime_t runtime, uint64_t doc,
   * struct iroh_bytes_t author_id, struct iroh_bytes_t service, struct iroh_bytes_t contract_id,
   * struct iroh_bytes_t endpoint_id, struct iroh_bytes_t health, float load, int32_t
   * lease_duration_s, uint64_t gossip_topic, uint64_t user_data, iroh_operation_t *out_operation);}
   */
  public int asterRegistryRenewLease(
      long runtimeHandle,
      long docHandle,
      MemorySegment authorIdPtr,
      long authorIdLen,
      MemorySegment servicePtr,
      long serviceLen,
      MemorySegment contractIdPtr,
      long contractIdLen,
      MemorySegment endpointIdPtr,
      long endpointIdLen,
      MemorySegment healthPtr,
      long healthLen,
      float load,
      int leaseDurationS,
      long gossipTopic,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "aster_registry_renew_lease",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG, // runtime
                      ValueLayout.JAVA_LONG, // doc
                      ValueLayout.ADDRESS, // author ptr
                      ValueLayout.JAVA_LONG, // author len
                      ValueLayout.ADDRESS, // service ptr
                      ValueLayout.JAVA_LONG, // service len
                      ValueLayout.ADDRESS, // contract id ptr
                      ValueLayout.JAVA_LONG, // contract id len
                      ValueLayout.ADDRESS, // endpoint id ptr
                      ValueLayout.JAVA_LONG, // endpoint id len
                      ValueLayout.ADDRESS, // health ptr
                      ValueLayout.JAVA_LONG, // health len
                      ValueLayout.JAVA_FLOAT, // load
                      ValueLayout.JAVA_INT, // lease duration
                      ValueLayout.JAVA_LONG, // gossip topic
                      ValueLayout.JAVA_LONG, // user data
                      ValueLayout.ADDRESS)) // out op
              .invoke(
                  runtimeHandle,
                  docHandle,
                  authorIdPtr,
                  authorIdLen,
                  servicePtr,
                  serviceLen,
                  contractIdPtr,
                  contractIdLen,
                  endpointIdPtr,
                  endpointIdLen,
                  healthPtr,
                  healthLen,
                  load,
                  leaseDurationS,
                  gossipTopic,
                  userData,
                  outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /** C signature: {@code int32_t aster_registry_acl_add_writer(...);} */
  public int asterRegistryAclAddWriter(
      long runtimeHandle,
      long docHandle,
      MemorySegment authorIdPtr,
      long authorIdLen,
      MemorySegment writerIdPtr,
      long writerIdLen,
      long userData,
      MemorySegment outOpId) {
    return aclMutateWriter(
        "aster_registry_acl_add_writer",
        runtimeHandle,
        docHandle,
        authorIdPtr,
        authorIdLen,
        writerIdPtr,
        writerIdLen,
        userData,
        outOpId);
  }

  /** C signature: {@code int32_t aster_registry_acl_remove_writer(...);} */
  public int asterRegistryAclRemoveWriter(
      long runtimeHandle,
      long docHandle,
      MemorySegment authorIdPtr,
      long authorIdLen,
      MemorySegment writerIdPtr,
      long writerIdLen,
      long userData,
      MemorySegment outOpId) {
    return aclMutateWriter(
        "aster_registry_acl_remove_writer",
        runtimeHandle,
        docHandle,
        authorIdPtr,
        authorIdLen,
        writerIdPtr,
        writerIdLen,
        userData,
        outOpId);
  }

  private int aclMutateWriter(
      String symbol,
      long runtimeHandle,
      long docHandle,
      MemorySegment authorIdPtr,
      long authorIdLen,
      MemorySegment writerIdPtr,
      long writerIdLen,
      long userData,
      MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  symbol,
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(
                  runtimeHandle,
                  docHandle,
                  authorIdPtr,
                  authorIdLen,
                  writerIdPtr,
                  writerIdLen,
                  userData,
                  outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /** C signature: {@code int32_t aster_registry_acl_list_writers(...);} */
  public int asterRegistryAclListWriters(
      long runtimeHandle, long docHandle, long userData, MemorySegment outOpId) {
    try {
      return (int)
          getHandle(
                  "aster_registry_acl_list_writers",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, docHandle, userData, outOpId);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // --- Hook responders (Phase 1b) ---------------------------------------

  /**
   * C signature: {@code int32_t iroh_hook_before_connect_respond(iroh_runtime_t runtime,
   * iroh_hook_invocation_t invocation, enum iroh_hook_decision_t decision);}
   */
  public int irohHookBeforeConnectRespond(long runtimeHandle, long invocation, int decision) {
    try {
      return (int)
          getHandle(
                  "iroh_hook_before_connect_respond",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_INT))
              .invoke(runtimeHandle, invocation, decision);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /**
   * C signature: {@code int32_t iroh_hook_after_connect_respond(iroh_runtime_t runtime,
   * iroh_hook_invocation_t invocation);}
   */
  public int irohHookAfterConnectRespond(long runtimeHandle, long invocation) {
    try {
      return (int)
          getHandle(
                  "iroh_hook_after_connect_respond",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG))
              .invoke(runtimeHandle, invocation);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }
}
