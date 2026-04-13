package site.aster.node;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.HexFormat;
import java.util.concurrent.*;
import java.util.function.Consumer;
import site.aster.blobs.IrohBlobs;
import site.aster.docs.Docs;
import site.aster.event.IrohEvent;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.gossip.IrohGossip;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohRuntime;
import site.aster.tags.IrohTags;

/**
 * High-level Iroh node with all protocols enabled.
 *
 * <p>Create via the factory methods: {@link #memory()}, {@link #memoryWithAlpns}, {@link
 * #persistent}, {@link #persistentWithAlpns}.
 *
 * <p>Connections from remote peers are delivered via {@link #connections} as a {@link Flow}. Close
 * the node with {@link #close}.
 */
public class IrohNode implements AutoCloseable {

  private final IrohRuntime runtime;
  private final long nodeHandle;
  private final Flow<AcceptedAster> connections;
  private final Consumer<IrohEvent> acceptHandler;

  /**
   * Flow of incoming Aster connections accepted by this node.
   *
   * <p>Each emitted {@link AcceptedAster} contains the ALPN bytes and the accepted {@link
   * IrohConnection}.
   *
   * <p>This is a {@link MutableSharedFlow} — multiple collectors all receive the same connection
   * events. The accept loop runs in the {@link IrohRuntime}'s {@link site.aster.ffi.IrohPollThread}
   * and stops when the node is closed.
   */
  public Flow<AcceptedAster> connections() {
    return connections;
  }

  private IrohNode(IrohRuntime runtime, long nodeHandle) {
    this.runtime = runtime;
    this.nodeHandle = nodeHandle;
    this.connections = new MutableSharedFlow<>(0, 64, BufferOverflow.SUSPEND);
    this.acceptHandler = this::onAcceptEvent;
    registerAcceptHandler();
  }

  private void registerAcceptHandler() {
    runtime.addInboundHandler(acceptHandler);
  }

  private void onAcceptEvent(IrohEvent event) {
    if (event.kind() != IrohEventKind.ASTER_ACCEPTED) return;
    AcceptedAster accepted = acceptedFromEvent(event);
    ((MutableSharedFlow<AcceptedAster>) connections).emit(accepted);
  }

  private AcceptedAster acceptedFromEvent(IrohEvent event) {
    long connHandle = event.handle();
    byte[] alpnBytes = extractAlpnFromEvent(event);
    IrohConnection conn = new IrohConnection(runtime, connHandle);
    return new AcceptedAster(alpnBytes, conn);
  }

  private byte[] extractAlpnFromEvent(IrohEvent event) {
    long bufLen = event.dataLen();
    if (bufLen <= 0) return new byte[0];

    MemorySegment data = event.data();
    byte[] bytes = data.asSlice(0, bufLen).toArray(ValueLayout.JAVA_BYTE);

    if (event.hasBuffer()) {
      runtime.releaseBuffer(event.buffer());
    }
    return bytes;
  }

  /** Node's endpoint ID as a hex string. */
  public String nodeId() {
    IrohLibrary lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    SegmentAllocator alloc = confined;
    MemorySegment bufSeg = alloc.allocate(ValueLayout.JAVA_BYTE, 64);
    MemorySegment lenSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    int status = lib.nodeId(runtime.nativeHandle(), nodeHandle, bufSeg, 64, lenSeg);
    if (status != 0) {
      throw IrohException.forStatus(IrohStatus.fromCode(status), "iroh_node_id failed");
    }

    int len = (int) lenSeg.get(ValueLayout.JAVA_LONG, 0);
    if (len == 0) return "";
    byte[] bytes = bufSeg.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
    return HexFormat.of().formatHex(bytes);
  }

  /** Structured node address info. */
  public NodeAddr nodeAddr() {
    IrohLibrary lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    SegmentAllocator alloc = confined;
    MemorySegment bufSeg = alloc.allocate(ValueLayout.JAVA_BYTE, 4096);
    MemorySegment addrSeg = alloc.allocate(IrohLibrary.IROH_NODE_ADDR);

    int status = lib.nodeAddrInfo(runtime.nativeHandle(), nodeHandle, bufSeg, 4096, addrSeg);
    if (status != 0) {
      throw IrohException.forStatus(IrohStatus.fromCode(status), "iroh_node_addr_info failed");
    }

    String endpointId = readIrohBytes(addrSeg.asSlice(0, 16));
    String relayUrl = readIrohBytesOpt(addrSeg.asSlice(16, 16));
    java.util.List<String> directAddresses = readIrohBytesList(addrSeg.asSlice(32, 16));

    return new NodeAddr(endpointId, relayUrl, directAddresses);
  }

  private String readIrohBytes(MemorySegment seg) {
    MemorySegment ptr = seg.get(ValueLayout.ADDRESS, 0);
    long len = seg.get(ValueLayout.JAVA_LONG, 8);
    if (ptr != MemorySegment.NULL && len > 0) {
      return StandardCharsets.UTF_8.decode(ptr.reinterpret(len).asByteBuffer()).toString();
    }
    return "";
  }

  private String readIrohBytesOpt(MemorySegment seg) {
    MemorySegment ptr = seg.get(ValueLayout.ADDRESS, 0);
    long len = seg.get(ValueLayout.JAVA_LONG, 8);
    if (ptr != MemorySegment.NULL && len > 0) {
      return StandardCharsets.UTF_8.decode(ptr.reinterpret(len).asByteBuffer()).toString();
    }
    return null;
  }

  private java.util.List<String> readIrohBytesList(MemorySegment seg) {
    MemorySegment itemsPtr = seg.get(ValueLayout.ADDRESS, 0);
    long itemsLen = seg.get(ValueLayout.JAVA_LONG, 8);
    if (itemsPtr == MemorySegment.NULL || itemsLen == 0) return java.util.Collections.emptyList();

    java.util.List<String> result = new java.util.ArrayList<>();
    long itemSize = 16; // size of iroh_bytes_t { ptr: 8, len: 8 }
    for (int i = 0; i < itemsLen; i++) {
      MemorySegment itemSeg = MemorySegment.ofAddress(itemsPtr.address() + i * itemSize);
      MemorySegment itemPtr = itemSeg.get(ValueLayout.ADDRESS, 0);
      long itemLen = itemSeg.get(ValueLayout.JAVA_LONG, 8);
      if (itemPtr != MemorySegment.NULL && itemLen > 0) {
        result.add(
            StandardCharsets.UTF_8.decode(itemPtr.reinterpret(itemLen).asByteBuffer()).toString());
      }
    }
    return result;
  }

  /** Export the node's 32-byte secret key seed. */
  public byte[] exportSecretKey() {
    IrohLibrary lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    SegmentAllocator alloc = confined;
    MemorySegment bufSeg = alloc.allocate(ValueLayout.JAVA_BYTE, 32);
    MemorySegment lenSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    int status = lib.nodeExportSecretKey(runtime.nativeHandle(), nodeHandle, bufSeg, 32, lenSeg);
    if (status != 0) {
      throw IrohException.forStatus(
          IrohStatus.fromCode(status), "iroh_node_export_secret_key failed");
    }

    int len = (int) lenSeg.get(ValueLayout.JAVA_LONG, 0);
    if (len == 0) return new byte[0];
    return bufSeg.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
  }

  /** Whether this node was built with hooks enabled. */
  public boolean hasHooks() {
    return false;
  }

  /** Close this node and free its handle. */
  public void close() {
    runtime.removeInboundHandler(acceptHandler);
    IrohLibrary lib = IrohLibrary.getInstance();
    lib.nodeFree(runtime.nativeHandle(), nodeHandle);
  }

  /**
   * Get the blob store associated with this node.
   *
   * @return the IrohBlobs instance
   */
  public IrohBlobs blobs() {
    return new IrohBlobs(runtime, nodeHandle);
  }

  /**
   * Get the tag store associated with this node.
   *
   * @return the IrohTags instance
   */
  public IrohTags tags() {
    return new IrohTags(runtime, nodeHandle);
  }

  /**
   * Get the document operations for this node.
   *
   * @return the Docs instance
   */
  public Docs docs() {
    return new Docs(runtime, nodeHandle);
  }

  /**
   * Get the gossip pub-sub operations for this node.
   *
   * @return the IrohGossip instance
   */
  public IrohGossip gossip() {
    return new IrohGossip(runtime, nodeHandle);
  }

  /** Accessor for the runtime. */
  public IrohRuntime runtime() {
    return runtime;
  }

  /** Accessor for the native node handle. */
  public long nodeHandle() {
    return nodeHandle;
  }

  // ============================================================================
  // Factory methods
  // ============================================================================

  /** Create an in-memory node with all protocols enabled. */
  public static CompletableFuture<IrohNode> memory() {
    return createMemoryNode(java.util.Collections.emptyList());
  }

  /**
   * Create an in-memory node that accepts connections on the given ALPNs.
   *
   * @param alpns list of ALPN protocol names (as bytes)
   */
  public static CompletableFuture<IrohNode> memoryWithAlpns(java.util.List<byte[]> alpns) {
    return createMemoryNode(
        alpns.stream().map(b -> new String(b, StandardCharsets.UTF_8)).toList());
  }

  /**
   * Create a persistent node at the given path with all protocols enabled.
   *
   * @param path directory path for persistent state
   */
  public static CompletableFuture<IrohNode> persistent(String path) {
    return createPersistentNode(path, java.util.Collections.emptyList());
  }

  /**
   * Create a persistent node at the given path that accepts connections on the given ALPNs.
   *
   * @param path directory path for persistent state
   * @param alpns list of ALPN protocol names (as bytes)
   */
  public static CompletableFuture<IrohNode> persistentWithAlpns(
      String path, java.util.List<byte[]> alpns) {
    return createPersistentNode(
        path, alpns.stream().map(b -> new String(b, StandardCharsets.UTF_8)).toList());
  }

  private static CompletableFuture<IrohNode> createMemoryNode(java.util.List<String> alpns) {
    IrohRuntime runtime = IrohRuntime.create();
    return CompletableFuture.supplyAsync(
        () -> {
          long nodeHandle = createNodeViaFfi(runtime, alpns, null);
          return new IrohNode(runtime, nodeHandle);
        },
        ForkJoinPool.commonPool());
  }

  private static CompletableFuture<IrohNode> createPersistentNode(
      String path, java.util.List<String> alpns) {
    IrohRuntime runtime = IrohRuntime.create();
    return CompletableFuture.supplyAsync(
        () -> {
          long nodeHandle = createNodeViaFfi(runtime, alpns, path);
          return new IrohNode(runtime, nodeHandle);
        },
        ForkJoinPool.commonPool());
  }

  private static long createNodeViaFfi(
      IrohRuntime runtime, java.util.List<String> alpns, String persistPath) {
    IrohLibrary lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    SegmentAllocator alloc = confined;
    MemorySegment opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    int status;
    if (persistPath != null) {
      byte[] pathBytes = persistPath.getBytes(StandardCharsets.UTF_8);
      MemorySegment pathSeg = alloc.allocate(ValueLayout.JAVA_BYTE, pathBytes.length + 1);
      pathSeg.copyFrom(MemorySegment.ofArray(pathBytes));
      pathSeg.set(ValueLayout.JAVA_BYTE, pathBytes.length, (byte) 0);
      status = lib.nodePersistentAsync(runtime.nativeHandle(), pathSeg, pathBytes.length, opSeg);
    } else if (alpns.isEmpty()) {
      status = lib.nodeMemoryAsync(runtime.nativeHandle(), opSeg);
    } else {
      int alpnCount = alpns.size();
      MemorySegment itemsSeg = alloc.allocate(ValueLayout.ADDRESS, alpnCount);
      MemorySegment lensSeg = alloc.allocate(ValueLayout.JAVA_LONG, alpnCount);

      for (int i = 0; i < alpnCount; i++) {
        byte[] alpnBytes = alpns.get(i).getBytes(StandardCharsets.UTF_8);
        MemorySegment alpnSeg = alloc.allocate(ValueLayout.JAVA_BYTE, alpnBytes.length);
        alpnSeg.copyFrom(MemorySegment.ofArray(alpnBytes));
        itemsSeg.set(ValueLayout.ADDRESS, i * 8, alpnSeg);
        lensSeg.set(ValueLayout.JAVA_LONG, i * 8, alpnBytes.length);
      }

      status =
          lib.nodeMemoryWithAlpnsAsync(runtime.nativeHandle(), itemsSeg, lensSeg, alpnCount, opSeg);
    }

    if (status != 0) {
      runtime.close();
      throw IrohException.forStatus(IrohStatus.fromCode(status), "node creation failed");
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    try {
      IrohEvent event = runtime.registry().register(opId).get();
      if (event.kind() != IrohEventKind.NODE_CREATED) {
        runtime.close();
        throw new IrohException("node creation failed: " + event.kind());
      }
      return event.handle();
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
      runtime.close();
      throw new IrohException("node creation interrupted");
    } catch (ExecutionException e) {
      runtime.close();
      throw new IrohException("node creation failed: " + e.getCause().getMessage());
    }
  }
}
