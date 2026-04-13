package site.aster.server;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.ffi.IrohException;
import site.aster.ffi.Reactor;
import site.aster.node.IrohNode;

/**
 * High-level Aster RPC server.
 *
 * <p>Creates an Iroh node with the {@code aster/1} ALPN, attaches a reactor, and runs a poll loop
 * that dispatches incoming RPC calls to a registered {@link CallHandler}.
 *
 * <p>Usage:
 *
 * <pre>{@code
 * AsterServer server = AsterServer.builder()
 *     .handler(call -> CallResponse.of(call.request()))  // echo
 *     .build()
 *     .get();
 *
 * System.out.println("Node ID: " + server.nodeId());
 * // ... server is accepting calls ...
 *
 * server.close();
 * }</pre>
 */
public final class AsterServer implements AutoCloseable {

  public static final String ASTER_ALPN = "aster/1";
  private static final int DEFAULT_RING_CAPACITY = 256;
  private static final int DEFAULT_POLL_BATCH = 32;
  private static final int POLL_TIMEOUT_MS = 100;

  private final IrohNode node;
  private final Reactor reactor;
  private final CallHandler handler;
  private final AtomicBoolean running = new AtomicBoolean(true);
  private final Thread pollThread;
  private final ExecutorService callExecutor;

  private AsterServer(IrohNode node, Reactor reactor, CallHandler handler) {
    this.node = node;
    this.reactor = reactor;
    this.handler = handler;
    this.callExecutor = Executors.newVirtualThreadPerTaskExecutor();
    this.pollThread =
        Thread.ofPlatform().daemon(true).name("aster-server-poll").start(this::pollLoop);
  }

  /** The node ID of this server (hex string). */
  public String nodeId() {
    return node.nodeId();
  }

  /** The underlying Iroh node. */
  public IrohNode node() {
    return node;
  }

  /** Stop the server, close the reactor and node. */
  @Override
  public void close() {
    if (!running.compareAndSet(true, false)) {
      return;
    }
    try {
      pollThread.join(2000);
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
    }
    callExecutor.shutdown();
    try {
      callExecutor.awaitTermination(2, TimeUnit.SECONDS);
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
    }
    reactor.close();
    node.close();
  }

  private void pollLoop() {
    long callSize = Reactor.CALL_LAYOUT.byteSize();
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment callBuffer = arena.allocate(Reactor.CALL_LAYOUT, DEFAULT_POLL_BATCH);

      while (running.get()) {
        int count = reactor.poll(callBuffer, DEFAULT_POLL_BATCH, POLL_TIMEOUT_MS);
        for (int i = 0; i < count; i++) {
          MemorySegment slot = callBuffer.asSlice(i * callSize, callSize);
          AsterCall call = extractCall(slot);
          long callId = call.callId();
          long headerBuf = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_HEADER_BUFFER);
          long requestBuf = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_REQUEST_BUFFER);
          long peerBuf = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_PEER_BUFFER);

          // Release buffers immediately — we've already copied the data
          reactor.bufferRelease(headerBuf);
          reactor.bufferRelease(requestBuf);
          reactor.bufferRelease(peerBuf);

          // Dispatch to handler on a virtual thread
          callExecutor.execute(() -> dispatchCall(callId, call));
        }
      }
    }
  }

  private AsterCall extractCall(MemorySegment slot) {
    long callId = slot.get(ValueLayout.JAVA_LONG, Reactor.OFFSET_CALL_ID);

    MemorySegment headerPtr = slot.get(ValueLayout.ADDRESS, Reactor.OFFSET_HEADER_PTR);
    int headerLen = slot.get(ValueLayout.JAVA_INT, Reactor.OFFSET_HEADER_LEN);
    byte headerFlags = slot.get(ValueLayout.JAVA_BYTE, Reactor.OFFSET_HEADER_FLAGS);

    MemorySegment requestPtr = slot.get(ValueLayout.ADDRESS, Reactor.OFFSET_REQUEST_PTR);
    int requestLen = slot.get(ValueLayout.JAVA_INT, Reactor.OFFSET_REQUEST_LEN);
    byte requestFlags = slot.get(ValueLayout.JAVA_BYTE, Reactor.OFFSET_REQUEST_FLAGS);

    MemorySegment peerPtr = slot.get(ValueLayout.ADDRESS, Reactor.OFFSET_PEER_PTR);
    int peerLen = slot.get(ValueLayout.JAVA_INT, Reactor.OFFSET_PEER_LEN);

    byte isSession = slot.get(ValueLayout.JAVA_BYTE, Reactor.OFFSET_IS_SESSION_CALL);

    // Copy data out of native memory before releasing buffers
    byte[] header =
        headerLen > 0
            ? headerPtr.reinterpret(headerLen).toArray(ValueLayout.JAVA_BYTE)
            : new byte[0];
    byte[] request =
        requestLen > 0
            ? requestPtr.reinterpret(requestLen).toArray(ValueLayout.JAVA_BYTE)
            : new byte[0];
    String peerId =
        peerLen > 0
            ? new String(
                peerPtr.reinterpret(peerLen).toArray(ValueLayout.JAVA_BYTE), StandardCharsets.UTF_8)
            : "";

    return new AsterCall(
        callId, header, headerFlags, request, requestFlags, peerId, isSession != 0);
  }

  private void dispatchCall(long callId, AsterCall call) {
    try {
      CallResponse response = handler.handle(call);
      submitResponse(callId, response);
    } catch (Exception e) {
      // Build a minimal error trailer and submit it
      byte[] errorBytes = ("ERROR: " + e.getMessage()).getBytes(StandardCharsets.UTF_8);
      submitResponse(callId, CallResponse.of(new byte[0], errorBytes));
    }
  }

  private void submitResponse(long callId, CallResponse response) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment respSeg = MemorySegment.NULL;
      MemorySegment trailerSeg = MemorySegment.NULL;

      if (response.responseFrame().length > 0) {
        respSeg = arena.allocate(ValueLayout.JAVA_BYTE, response.responseFrame().length);
        respSeg.copyFrom(MemorySegment.ofArray(response.responseFrame()));
      }
      if (response.trailerFrame().length > 0) {
        trailerSeg = arena.allocate(ValueLayout.JAVA_BYTE, response.trailerFrame().length);
        trailerSeg.copyFrom(MemorySegment.ofArray(response.trailerFrame()));
      }

      reactor.submit(callId, respSeg, trailerSeg);
    }
  }

  // ============================================================================
  // Builder
  // ============================================================================

  /** Create a new builder for an AsterServer. */
  public static Builder builder() {
    return new Builder();
  }

  /** Builder for {@link AsterServer}. */
  public static final class Builder {
    private CallHandler handler;
    private List<String> alpns = List.of(ASTER_ALPN);
    private int ringCapacity = DEFAULT_RING_CAPACITY;

    private Builder() {}

    /** Set the call handler (required). */
    public Builder handler(CallHandler handler) {
      this.handler = handler;
      return this;
    }

    /** Set additional ALPNs (aster/1 is always included). */
    public Builder alpns(List<String> extraAlpns) {
      var all = new java.util.ArrayList<>(List.of(ASTER_ALPN));
      for (String a : extraAlpns) {
        if (!a.equals(ASTER_ALPN)) {
          all.add(a);
        }
      }
      this.alpns = List.copyOf(all);
      return this;
    }

    /** Set the reactor ring buffer capacity (default 256). */
    public Builder ringCapacity(int capacity) {
      this.ringCapacity = capacity;
      return this;
    }

    /**
     * Build the server asynchronously. The returned future completes when the node is ready and the
     * reactor is accepting calls.
     */
    public CompletableFuture<AsterServer> build() {
      if (handler == null) {
        throw new IllegalStateException("handler must be set");
      }
      CallHandler h = handler;
      int cap = ringCapacity;
      List<byte[]> alpnBytes = alpns.stream().map(s -> s.getBytes(StandardCharsets.UTF_8)).toList();

      return IrohNode.memoryWithAlpns(alpnBytes)
          .thenApply(
              node -> {
                try {
                  Reactor reactor =
                      new Reactor(node.runtime().nativeHandle(), node.nodeHandle(), cap);
                  return new AsterServer(node, reactor, h);
                } catch (Exception e) {
                  node.close();
                  throw new IrohException("Failed to create reactor: " + e.getMessage());
                }
              });
    }
  }
}
