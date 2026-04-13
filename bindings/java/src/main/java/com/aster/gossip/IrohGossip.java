package com.aster.gossip;

import com.aster.ffi.IrohEventKind;
import com.aster.ffi.IrohException;
import com.aster.ffi.IrohLibrary;
import com.aster.ffi.IrohStatus;
import com.aster.handle.IrohRuntime;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.CompletableFuture;

/**
 * Gossip pub-sub operations for an Iroh node.
 *
 * <p>Get an instance via {@link com.aster.node.IrohNode#gossip}.
 */
public class IrohGossip {

  private final IrohRuntime runtime;
  private final long nodeHandle;

  public IrohGossip(IrohRuntime runtime, long nodeHandle) {
    this.runtime = runtime;
    this.nodeHandle = nodeHandle;
  }

  /**
   * Subscribe to a gossip topic.
   *
   * @param topic the topic name
   * @param peers list of peer node IDs (hex strings) to join
   * @return a future that completes with a GossipTopic handle
   */
  public CompletableFuture<GossipTopic> subscribeAsync(String topic, List<String> peers) {
    var lib = IrohLibrary.getInstance();
    Arena confined = Arena.ofConfined();
    SegmentAllocator alloc = confined;

    // Marshal topic as iroh_bytes_t (struct by value: ptr + len)
    byte[] topicBytes = topic.getBytes(StandardCharsets.UTF_8);
    MemorySegment topicSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
    MemorySegment topicDataSeg = alloc.allocate(topicBytes.length);
    topicDataSeg.copyFrom(MemorySegment.ofArray(topicBytes));
    topicSeg.set(ValueLayout.ADDRESS, 0, topicDataSeg);
    topicSeg.set(ValueLayout.JAVA_LONG, 8, (long) topicBytes.length);

    // Marshal peers as iroh_bytes_list_t (struct by value: items ptr + len)
    // Each item is an iroh_bytes_t struct { ptr, len } = 16 bytes
    int peerCount = peers.size();
    MemorySegment peersSeg = alloc.allocate(IrohLibrary.IROH_BYTES_LIST);
    if (peerCount > 0) {
      MemorySegment itemsSeg = alloc.allocate(IrohLibrary.IROH_BYTES, peerCount);
      for (int i = 0; i < peerCount; i++) {
        byte[] peerBytes = peers.get(i).getBytes(StandardCharsets.UTF_8);
        MemorySegment peerDataSeg = alloc.allocate(peerBytes.length);
        peerDataSeg.copyFrom(MemorySegment.ofArray(peerBytes));
        long offset = i * IrohLibrary.IROH_BYTES.byteSize();
        itemsSeg.set(ValueLayout.ADDRESS, offset, peerDataSeg);
        itemsSeg.set(ValueLayout.JAVA_LONG, offset + 8, (long) peerBytes.length);
      }
      peersSeg.set(ValueLayout.ADDRESS, 0, itemsSeg);
      peersSeg.set(ValueLayout.JAVA_LONG, 8, (long) peerCount);
    } else {
      peersSeg.set(ValueLayout.ADDRESS, 0, MemorySegment.NULL);
      peersSeg.set(ValueLayout.JAVA_LONG, 8, 0L);
    }

    var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

    try {
      int status =
          lib.gossipSubscribe(runtime.nativeHandle(), nodeHandle, topicSeg, peersSeg, opSeg);
      if (status != 0) {
        throw new IrohException(
            IrohStatus.fromCode(status), "iroh_gossip_subscribe failed: " + status);
      }
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(
          new IrohException("iroh_gossip_subscribe threw: " + t.getMessage()));
    }

    long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            event -> {
              if (event.kind() == IrohEventKind.GOSSIP_SUBSCRIBED) {
                long topicHandle = event.handle();
                return new GossipTopic(runtime, topicHandle);
              }
              throw new IrohException("subscribe failed: unexpected event " + event.kind());
            });
  }

  /** Handle to a subscribed gossip topic. Supports broadcast, receive, and close. */
  public static class GossipTopic implements AutoCloseable {

    private final IrohRuntime runtime;
    private final long topicHandle;

    GossipTopic(IrohRuntime runtime, long topicHandle) {
      this.runtime = runtime;
      this.topicHandle = topicHandle;
    }

    /** Returns the native topic handle. */
    public long handle() {
      return topicHandle;
    }

    /**
     * Broadcast data to all peers subscribed to this topic.
     *
     * @param data the bytes to broadcast
     * @return a future that completes when the broadcast is done
     */
    public CompletableFuture<Void> broadcastAsync(byte[] data) {
      var lib = IrohLibrary.getInstance();
      Arena confined = Arena.ofConfined();
      SegmentAllocator alloc = confined;

      // Marshal data as iroh_bytes_t
      MemorySegment dataSeg = alloc.allocate(IrohLibrary.IROH_BYTES);
      MemorySegment heapSeg = alloc.allocate(data.length);
      heapSeg.copyFrom(MemorySegment.ofArray(data));
      dataSeg.set(ValueLayout.ADDRESS, 0, heapSeg);
      dataSeg.set(ValueLayout.JAVA_LONG, 8, (long) data.length);

      var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

      try {
        int status = lib.gossipBroadcast(runtime.nativeHandle(), topicHandle, dataSeg, opSeg);
        if (status != 0) {
          throw new IrohException(
              IrohStatus.fromCode(status), "iroh_gossip_broadcast failed: " + status);
        }
      } catch (IrohException e) {
        return CompletableFuture.failedFuture(e);
      } catch (Throwable t) {
        return CompletableFuture.failedFuture(
            new IrohException("iroh_gossip_broadcast threw: " + t.getMessage()));
      }

      long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
      return runtime
          .registry()
          .register(opId)
          .thenApply(
              event -> {
                if (event.kind() == IrohEventKind.GOSSIP_BROADCAST_DONE) {
                  return null;
                }
                throw new IrohException("broadcast failed: unexpected event " + event.kind());
              });
    }

    /**
     * Receive the next message from this topic.
     *
     * @return a future that completes with the received message bytes
     */
    public CompletableFuture<byte[]> recvAsync() {
      var lib = IrohLibrary.getInstance();
      Arena confined = Arena.ofConfined();
      SegmentAllocator alloc = confined;

      var opSeg = alloc.allocate(ValueLayout.JAVA_LONG);

      try {
        int status = lib.gossipRecv(runtime.nativeHandle(), topicHandle, opSeg);
        if (status != 0) {
          throw new IrohException(
              IrohStatus.fromCode(status), "iroh_gossip_recv failed: " + status);
        }
      } catch (IrohException e) {
        return CompletableFuture.failedFuture(e);
      } catch (Throwable t) {
        return CompletableFuture.failedFuture(
            new IrohException("iroh_gossip_recv threw: " + t.getMessage()));
      }

      long opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
      return runtime
          .registry()
          .register(opId)
          .thenApply(
              event -> {
                if (event.kind() == IrohEventKind.GOSSIP_RECEIVED) {
                  byte[] data = event.data().asByteBuffer().array();
                  if (event.hasBuffer()) {
                    runtime.releaseBuffer(event.buffer());
                  }
                  return data;
                }
                throw new IrohException("recv failed: unexpected event " + event.kind());
              });
    }

    /** Free the native gossip topic handle. */
    @Override
    public void close() {
      IrohLibrary lib = IrohLibrary.getInstance();
      lib.gossipTopicFree(runtime.nativeHandle(), topicHandle);
    }
  }
}
