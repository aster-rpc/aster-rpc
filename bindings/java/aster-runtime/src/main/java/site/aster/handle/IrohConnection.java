package site.aster.handle;

import java.lang.foreign.*;
import java.lang.foreign.MemoryLayout.PathElement;
import java.lang.invoke.VarHandle;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;
import site.aster.ffi.*;

public class IrohConnection extends IrohHandle {

  private final IrohRuntime runtime;

  /**
   * Monotonic per-connection session-id allocator (spec §6). Starts at 0. {@link #nextSessionId()}
   * returns {@code counter.incrementAndGet()}, so the first session is {@code 1}, the second {@code
   * 2}, and so on — {@code 0} is reserved for the SHARED (stateless) pool.
   */
  private final AtomicInteger sessionIdAllocator = new AtomicInteger(0);

  public IrohConnection(IrohRuntime runtime, long handle) {
    super(handle);
    this.runtime = runtime;
  }

  /**
   * Allocate the next session id on this connection. Guaranteed monotonic and non-zero. The spec
   * requires a {@code u32}; Java's signed {@code int} gives 2^31 - 1 values before wraparound,
   * which is well beyond any realistic workload.
   */
  public int nextSessionId() {
    return sessionIdAllocator.incrementAndGet();
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
   * Get the remote peer's node ID as a 64-char hex string.
   *
   * <p>The FFI's {@code iroh_connection_remote_id} returns the id already hex-formatted — the Rust
   * side calls {@code NodeId::to_string()} and copies those ASCII bytes into the output buffer — so
   * the Java wrapper decodes them as UTF-8 rather than hex-encoding again. Re-hexing was a
   * long-standing bug that doubled the length to 128 chars, diverging from the single-hex form the
   * reactor's peer_id carries.
   */
  public String remoteId() {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    var alloc = confined;

    // Hex-encoded NodeId is 64 ASCII bytes.
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
    return new String(bytes, java.nio.charset.StandardCharsets.UTF_8);
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
   * Snapshot of the currently selected QUIC path. Cheap; safe to call once per RPC dispatch.
   * Returns a snapshot whose {@link TransportSnapshot#peerAddr()} and {@link
   * TransportSnapshot#relayUrl()} are mutually exclusive — exactly one is non-null when a path is
   * selected. Both are null only if the connection has been dropped or no path is selected yet.
   */
  public TransportSnapshot transportSnapshot() {
    return transportSnapshot(runtime, nativeHandle());
  }

  /**
   * Snapshot the selected QUIC path for an {@code iroh_connection_t} handle directly. Used by
   * callers that hold a borrowed handle (e.g. {@link
   * site.aster.server.AsterCall#connectionHandle()} delivered by the reactor) and don't want to
   * construct a transient {@link IrohConnection} wrapper just to read the snapshot.
   */
  public static TransportSnapshot transportSnapshot(IrohRuntime runtime, long connectionHandle) {
    var lib = IrohLibrary.getInstance();
    try (Arena confined = Arena.ofConfined()) {
      var snapSeg = confined.allocate(IrohLibrary.IROH_TRANSPORT_SNAPSHOT);
      int status =
          lib.connectionTransportSnapshot(runtime.nativeHandle(), connectionHandle, snapSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_connection_transport_snapshot failed: " + status);
      }

      int pathKind = (int) SNAPSHOT_PATH_KIND.get(snapSeg, 0L);
      MemorySegment peerHostPtr = (MemorySegment) SNAPSHOT_PEER_HOST_PTR.get(snapSeg, 0L);
      long peerHostLen = (long) SNAPSHOT_PEER_HOST_LEN.get(snapSeg, 0L);
      int peerPort = Short.toUnsignedInt((short) SNAPSHOT_PEER_PORT.get(snapSeg, 0L));
      MemorySegment relayUrlPtr = (MemorySegment) SNAPSHOT_RELAY_URL_PTR.get(snapSeg, 0L);
      long relayUrlLen = (long) SNAPSHOT_RELAY_URL_LEN.get(snapSeg, 0L);
      long rttMicros = (long) SNAPSHOT_RTT_MICROS.get(snapSeg, 0L);

      InetSocketAddress peerAddr = null;
      String relayUrl = null;
      try {
        if (pathKind == 1 && peerHostLen > 0) {
          var hostBytes =
              peerHostPtr
                  .reinterpret(peerHostLen)
                  .asSlice(0, peerHostLen)
                  .toArray(ValueLayout.JAVA_BYTE);
          peerAddr =
              InetSocketAddress.createUnresolved(
                  new String(hostBytes, StandardCharsets.UTF_8), peerPort);
        } else if (pathKind == 2 && relayUrlLen > 0) {
          var urlBytes =
              relayUrlPtr
                  .reinterpret(relayUrlLen)
                  .asSlice(0, relayUrlLen)
                  .toArray(ValueLayout.JAVA_BYTE);
          relayUrl = new String(urlBytes, StandardCharsets.UTF_8);
        }
      } finally {
        // Always free both buffers (release on a null/zero-length buffer
        // is a safe no-op on the Rust side).
        lib.stringRelease(peerHostPtr, peerHostLen);
        lib.stringRelease(relayUrlPtr, relayUrlLen);
      }

      Duration rtt = rttMicros == Long.MAX_VALUE ? null : Duration.ofNanos(rttMicros * 1_000L);
      return new TransportSnapshot(peerAddr, relayUrl, rtt);
    }
  }

  // VarHandles into iroh_transport_snapshot_t for transportSnapshot().
  private static final VarHandle SNAPSHOT_PATH_KIND =
      IrohLibrary.IROH_TRANSPORT_SNAPSHOT.varHandle(PathElement.groupElement("path_kind"));
  private static final VarHandle SNAPSHOT_PEER_HOST_PTR =
      IrohLibrary.IROH_TRANSPORT_SNAPSHOT.varHandle(
          PathElement.groupElement("peer_host"), PathElement.groupElement("ptr"));
  private static final VarHandle SNAPSHOT_PEER_HOST_LEN =
      IrohLibrary.IROH_TRANSPORT_SNAPSHOT.varHandle(
          PathElement.groupElement("peer_host"), PathElement.groupElement("len"));
  private static final VarHandle SNAPSHOT_PEER_PORT =
      IrohLibrary.IROH_TRANSPORT_SNAPSHOT.varHandle(PathElement.groupElement("peer_port"));
  private static final VarHandle SNAPSHOT_RELAY_URL_PTR =
      IrohLibrary.IROH_TRANSPORT_SNAPSHOT.varHandle(
          PathElement.groupElement("relay_url"), PathElement.groupElement("ptr"));
  private static final VarHandle SNAPSHOT_RELAY_URL_LEN =
      IrohLibrary.IROH_TRANSPORT_SNAPSHOT.varHandle(
          PathElement.groupElement("relay_url"), PathElement.groupElement("len"));
  private static final VarHandle SNAPSHOT_RTT_MICROS =
      IrohLibrary.IROH_TRANSPORT_SNAPSHOT.varHandle(PathElement.groupElement("rtt_micros"));

  /** Snapshot of the selected QUIC path returned by {@link #transportSnapshot()}. */
  public record TransportSnapshot(InetSocketAddress peerAddr, String relayUrl, Duration rtt) {}

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
