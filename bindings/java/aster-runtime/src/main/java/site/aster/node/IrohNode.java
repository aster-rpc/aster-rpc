package site.aster.node;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.*;
import java.util.function.Consumer;
import site.aster.blobs.IrohBlobs;
import site.aster.config.AsterConfig;
import site.aster.docs.Docs;
import site.aster.event.IrohEvent;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.ffi.NativeConfig;
import site.aster.gossip.IrohGossip;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohRuntime;
import site.aster.tags.IrohTags;

/**
 * High-level Iroh node with all protocols enabled.
 *
 * <p>Create with {@link #start(AsterConfig)} (C ABI {@code aster_node_start}): the config carries
 * relay mode, bind address, secret key / key file, storage, hooks and transport tuning. The older
 * factories ({@link #memory()}, {@link #memoryWithAlpns}, {@link #persistent}, {@link
 * #persistentWithAlpns}) are shorthands over it.
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

  private final boolean hooks;

  private IrohNode(IrohRuntime runtime, long nodeHandle, boolean hooks) {
    this.runtime = runtime;
    this.nodeHandle = nodeHandle;
    this.hooks = hooks;
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

  /**
   * Node's endpoint ID as a 64-char hex string. The FFI returns already-hex-encoded ASCII bytes; we
   * decode as UTF-8 rather than hex-encode again (the latter would double-hex to 128 chars).
   */
  public String nodeId() {
    IrohLibrary lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    SegmentAllocator alloc = confined;
    MemorySegment bufSeg = alloc.allocate(ValueLayout.JAVA_BYTE, 64);
    MemorySegment lenSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    int status = lib.nodeId(runtime.nativeHandle(), nodeHandle, bufSeg, 64, lenSeg);
    if (status != 0) {
      throw IrohException.forStatus(IrohStatus.fromCode(status), "aster_node_id failed");
    }

    int len = (int) lenSeg.get(ValueLayout.JAVA_LONG, 0);
    if (len == 0) return "";
    byte[] bytes = bufSeg.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
    return new String(bytes, java.nio.charset.StandardCharsets.UTF_8);
  }

  /** Structured node address info. */
  public NodeAddr nodeAddr() {
    IrohLibrary lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    SegmentAllocator alloc = confined;
    MemorySegment bufSeg = alloc.allocate(ValueLayout.JAVA_BYTE, 4096);
    MemorySegment addrSeg = alloc.allocate(IrohLibrary.ASTER_NODE_ADDR);

    int status = lib.nodeAddrInfo(runtime.nativeHandle(), nodeHandle, bufSeg, 4096, addrSeg);
    if (status != 0) {
      throw IrohException.forStatus(IrohStatus.fromCode(status), "aster_node_addr_info failed");
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
    long itemSize = 16; // size of aster_bytes_t { ptr: 8, len: 8 }
    for (int i = 0; i < itemsLen; i++) {
      // ofAddress() yields a zero-length segment; size it to one aster_bytes_t before reading.
      MemorySegment itemSeg =
          MemorySegment.ofAddress(itemsPtr.address() + i * itemSize).reinterpret(itemSize);
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
          IrohStatus.fromCode(status), "aster_node_export_secret_key failed");
    }

    int len = (int) lenSeg.get(ValueLayout.JAVA_LONG, 0);
    if (len == 0) return new byte[0];
    return bufSeg.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
  }

  /** Whether this node was built with hooks enabled (or a gate). */
  public boolean hasHooks() {
    return hooks;
  }

  /**
   * Attach (or, with {@code null}, detach) a Gate-0 policy. Requires a node started with hooks or a
   * gate. Inbound peers are then admitted in Rust by the gate's rules.
   */
  public void setGate(Gate gate) {
    int status =
        IrohLibrary.getInstance()
            .nodeSetGate(runtime.nativeHandle(), nodeHandle, gate == null ? 0 : gate.handle());
    if (status != 0) {
      throw IrohException.forStatus(IrohStatus.fromCode(status), "aster_node_set_gate failed");
    }
  }

  /** Transport metrics in Prometheus text format ({@code aster_node_metrics_prometheus}). */
  public String metricsPrometheus() {
    IrohLibrary lib = IrohLibrary.getInstance();
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(IrohLibrary.ASTER_BYTES);
      int status = lib.nodeMetricsPrometheus(runtime.nativeHandle(), nodeHandle, out);
      if (status != 0) {
        throw IrohException.forStatus(
            IrohStatus.fromCode(status), "aster_node_metrics_prometheus failed");
      }
      MemorySegment ptr = out.get(ValueLayout.ADDRESS, 0);
      long len = out.get(ValueLayout.JAVA_LONG, 8);
      if (ptr.equals(MemorySegment.NULL) || len == 0) return "";
      String text =
          new String(ptr.reinterpret(len).toArray(ValueLayout.JAVA_BYTE), StandardCharsets.UTF_8);
      lib.stringRelease(ptr, len);
      return text;
    }
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

  /**
   * Start a node from {@code config} (C ABI {@code aster_node_start}). In-memory unless the config
   * sets a storage path. The config's ALPNs are registered for inbound connections.
   */
  public static CompletableFuture<IrohNode> start(AsterConfig config) {
    return start(config, java.util.Collections.emptyList(), null);
  }

  /**
   * Start a node from {@code config} that also accepts {@code alpns}, optionally gated by {@code
   * gate} (Gate 0 decided in Rust; forces hooks on). {@code config} may be null (defaults).
   */
  public static CompletableFuture<IrohNode> start(
      AsterConfig config, java.util.List<byte[]> alpns, Gate gate) {
    IrohRuntime runtime = gate != null ? gate.runtime() : IrohRuntime.create();
    return CompletableFuture.supplyAsync(
        () -> {
          try {
            long handle =
                NativeConfig.startNode(runtime, config, alpns, gate == null ? 0 : gate.handle());
            boolean hooks = gate != null || (config != null && config.enableHooks());
            return new IrohNode(runtime, handle, hooks);
          } catch (RuntimeException e) {
            if (gate == null) runtime.close();
            throw e;
          }
        },
        ForkJoinPool.commonPool());
  }

  /** Create an in-memory node with all protocols enabled (default config). */
  public static CompletableFuture<IrohNode> memory() {
    return start(null);
  }

  /**
   * Create an in-memory node that accepts connections on the given ALPNs (default config).
   *
   * @param alpns list of ALPN protocol names (as bytes)
   */
  public static CompletableFuture<IrohNode> memoryWithAlpns(java.util.List<byte[]> alpns) {
    return start(null, alpns, null);
  }

  /**
   * Create a persistent node at the given path with all protocols enabled.
   *
   * @param path directory path for persistent state
   */
  public static CompletableFuture<IrohNode> persistent(String path) {
    return persistentWithAlpns(path, java.util.Collections.emptyList());
  }

  /**
   * Create a persistent node at the given path that accepts connections on the given ALPNs.
   *
   * @param path directory path for persistent state
   * @param alpns list of ALPN protocol names (as bytes)
   */
  public static CompletableFuture<IrohNode> persistentWithAlpns(
      String path, java.util.List<byte[]> alpns) {
    return start(AsterConfig.builder().storagePath(path).build(), alpns, null);
  }

  /**
   * Make {@code addr} reachable from this node (C ABI {@code aster_add_node_addr}, nodes accepted
   * since ABI 3.3), so later dials and blob/doc fetches by node id can reach it without discovery.
   */
  public void addNodeAddr(NodeAddr addr) {
    site.aster.ffi.DataPlane.addNodeAddr(
        runtime,
        nodeHandle,
        addr.endpointId(),
        addr.relayUrl(),
        addr.directAddresses() == null ? java.util.List.of() : addr.directAddresses());
  }
}
