package site.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.atomic.LongAdder;

/**
 * Java FFM wrapper for the dynamic-dispatch C ABI (ABI 3.5): {@code aster_server_*} and {@code
 * aster_dispatch_*} (design {@code docs/_internal/bindings/dynamic-dispatch-design.md} §4.4).
 *
 * <p>Rust serves the registered services through {@code aster::rpc::Server}: it decodes headers,
 * routes, runs Gate 3 ({@code requires}), keeps sessions per connection, enforces deadlines and
 * per-method frame limits, and serves the baseline services. Java registers each service as its
 * JSON registration form ({@link Builder#addServiceJson}), drains {@link #poll} on one thread and
 * answers each call by its {@code call_id} with payloads and AEv1 {@code RpcStatus} bytes — never
 * frames.
 *
 * <p>Response operations never block in native code. A full response queue returns {@code
 * QUEUE_FULL}; this class then reserves the exact byte count ({@code aster_dispatch_reserve}) and
 * parks the producing (virtual) thread on a future that the poll thread completes on {@code
 * RESPONSE_WRITABLE} / {@code RESPONSE_CLOSED} ({@link #handleCapacityEvent}). The retry consumes
 * the held capacity, so it cannot be full again (the park logic formerly in {@code Reactor}).
 */
public final class Dispatch implements AutoCloseable {

  // ── aster_dispatch_event_t (104 bytes, ffi/src/layout.rs) ────────────────

  public static final MemoryLayout EVENT_LAYOUT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_BYTE.withName("kind"), //                     0
          ValueLayout.JAVA_BYTE.withName("pattern"), //                  1
          ValueLayout.JAVA_BYTE.withName("reason"), //                   2
          MemoryLayout.paddingLayout(1), //                              3
          ValueLayout.JAVA_INT.withName("flags"), //                     4
          ValueLayout.JAVA_LONG.withName("call_id"), //                  8
          ValueLayout.JAVA_INT.withName("service_idx"), //              16
          ValueLayout.JAVA_INT.withName("method_idx"), //               20
          ValueLayout.JAVA_LONG.withName("session_key"), //             24
          ValueLayout.JAVA_LONG.withName("connection_id"), //           32
          ValueLayout.JAVA_LONG.withName("connection_handle"), //       40
          ValueLayout.JAVA_LONG.withName("deadline_unix_ms"), //        48
          ValueLayout.JAVA_INT.withName("wire_call_id"), //             56
          ValueLayout.JAVA_INT.withName("session_id"), //               60
          ValueLayout.ADDRESS.withName("request_ptr"), //               64
          ValueLayout.JAVA_INT.withName("request_len"), //              72
          MemoryLayout.paddingLayout(4), //                             76
          ValueLayout.ADDRESS.withName("peer_ptr"), //                  80
          ValueLayout.JAVA_INT.withName("peer_len"), //                 88
          MemoryLayout.paddingLayout(4), //                             92
          ValueLayout.JAVA_LONG.withName("buffer") //                   96
          );

  public static final long OFFSET_KIND = 0;
  public static final long OFFSET_PATTERN = 1;
  public static final long OFFSET_REASON = 2;
  public static final long OFFSET_FLAGS = 4;
  public static final long OFFSET_CALL_ID = 8;
  public static final long OFFSET_SERVICE_IDX = 16;
  public static final long OFFSET_METHOD_IDX = 20;
  public static final long OFFSET_SESSION_KEY = 24;
  public static final long OFFSET_CONNECTION_ID = 32;
  public static final long OFFSET_CONNECTION_HANDLE = 40;
  public static final long OFFSET_DEADLINE_UNIX_MS = 48;
  public static final long OFFSET_WIRE_CALL_ID = 56;
  public static final long OFFSET_SESSION_ID = 60;
  public static final long OFFSET_REQUEST_PTR = 64;
  public static final long OFFSET_REQUEST_LEN = 72;
  public static final long OFFSET_PEER_PTR = 80;
  public static final long OFFSET_PEER_LEN = 88;
  public static final long OFFSET_BUFFER = 96;

  /** {@code aster_dispatch_event_kind_t}. */
  public static final byte EVENT_CALL = 0;

  public static final byte EVENT_CANCELLED = 1;
  public static final byte EVENT_RESPONSE_WRITABLE = 2;
  public static final byte EVENT_RESPONSE_CLOSED = 3;
  public static final byte EVENT_SESSION_CLOSED = 4;
  public static final byte EVENT_CONNECTION_CLOSED = 5;
  public static final byte EVENT_NON_RPC = 6;

  /** {@code aster_cancel_reason_t}. */
  public static final byte REASON_PEER = 0;

  public static final byte REASON_DEADLINE = 1;
  public static final byte REASON_INVALID_REQUEST = 2;

  /** {@code ASTER_DISPATCH_FLAG_*}. */
  public static final int FLAG_FIRST_IS_END = 0x1;

  public static final int FLAG_NEW_SESSION = 0x2;
  public static final int FLAG_IROH_AUTHENTICATED = 0x4;
  public static final int FLAG_HAS_METADATA = 0x100;
  public static final int FLAG_HAS_ATTRIBUTES = 0x200;

  /** {@code aster_server_option_t}. */
  public static final int OPTION_BUILTIN_NODE_INFO = 1;

  public static final int OPTION_BUILTIN_TOPOLOGY = 2;
  public static final int OPTION_MAX_SESSIONS_PER_CONNECTION = 3;
  public static final int OPTION_HANDLER_TIMEOUT_MS = 4;

  /**
   * Slice for {@link #recv}: a native wait never exceeds this before the handler's own cancellation
   * (call cancelled / over, thread interrupted) is re-checked. No {@code ASTER_TIMEOUT_INFINITE}.
   */
  public static final int RECV_SLICE_MS = 200;

  // ── method handles ───────────────────────────────────────────────────────

  private static final ValueLayout.OfInt I32 = ValueLayout.JAVA_INT;
  private static final ValueLayout.OfLong I64 = ValueLayout.JAVA_LONG;
  private static final ValueLayout.OfByte U8 = ValueLayout.JAVA_BYTE;
  private static final java.lang.foreign.AddressLayout PTR = ValueLayout.ADDRESS;

  private static MethodHandle h(String name, FunctionDescriptor d) {
    return IrohLibrary.getInstance().getHandle(name, d);
  }

  /** Short, never-blocking functions: critical, with heap segments allowed. */
  private static MethodHandle critical(String name, FunctionDescriptor d) {
    return IrohLibrary.getInstance().getHandle(name, d, Linker.Option.critical(true));
  }

  private static final MethodHandle BUILDER_NEW =
      h("aster_server_builder_new", FunctionDescriptor.of(I32, I64, I64, PTR));
  private static final MethodHandle ADD_SERVICE_JSON =
      h("aster_server_add_service_json", FunctionDescriptor.of(I32, I64, PTR, I32, PTR));
  private static final MethodHandle SET_OPTION =
      h("aster_server_set_option", FunctionDescriptor.of(I32, I64, I32, I64));
  private static final MethodHandle BUILDER_FREE =
      h("aster_server_builder_free", FunctionDescriptor.of(I32, I64));
  private static final MethodHandle START =
      h("aster_server_start", FunctionDescriptor.of(I32, I64, I64, I32, PTR));
  private static final MethodHandle DESTROY =
      h("aster_server_destroy", FunctionDescriptor.of(I32, I64, I64));
  private static final MethodHandle POLL =
      h("aster_server_poll", FunctionDescriptor.of(I32, I64, I64, PTR, I32, I32));
  private static final MethodHandle SET_PEER_ATTRIBUTES =
      h("aster_server_set_peer_attributes", FunctionDescriptor.of(I32, I64, PTR, I32, PTR, I32));
  private static final MethodHandle LIVE_CALLS =
      h("aster_server_live_calls", FunctionDescriptor.of(I64, I64));
  private static final MethodHandle CONTRACT_ID =
      h("aster_dynamic_service_contract_id", FunctionDescriptor.of(I32, PTR, I32, PTR));
  private static final MethodHandle STRING_RELEASE =
      h("aster_string_release", FunctionDescriptor.of(I32, PTR, I64));

  private static final MethodHandle RESPOND =
      critical(
          "aster_dispatch_respond",
          FunctionDescriptor.of(I32, I64, I64, I64, PTR, I32, PTR, I32, PTR));
  private static final MethodHandle SEND =
      critical("aster_dispatch_send", FunctionDescriptor.of(I32, I64, I64, I64, PTR, I32, PTR));
  private static final MethodHandle FINISH =
      critical("aster_dispatch_finish", FunctionDescriptor.of(I32, I64, I64, I64, PTR, I32, PTR));
  private static final MethodHandle RESERVE =
      critical("aster_dispatch_reserve", FunctionDescriptor.of(I32, I64, I64, I64, I64));
  private static final MethodHandle CANCEL_RESERVATION =
      critical("aster_dispatch_cancel_reservation", FunctionDescriptor.of(I32, I64, I64, I64));
  private static final MethodHandle IS_CANCELLED =
      critical("aster_dispatch_is_cancelled", FunctionDescriptor.of(I32, I64, I64, I64));
  private static final MethodHandle ABANDON =
      h("aster_dispatch_abandon", FunctionDescriptor.of(I32, I64, I64, I64));
  private static final MethodHandle BUFFER_RELEASE =
      critical("aster_dispatch_buffer_release", FunctionDescriptor.of(I32, I64, I64, I64));
  private static final MethodHandle RECV =
      h("aster_dispatch_recv", FunctionDescriptor.of(I32, I64, I64, I64, I32, PTR, PTR, PTR));
  private static final MethodHandle METADATA =
      h("aster_dispatch_metadata", FunctionDescriptor.of(I32, I64, I64, I64, PTR, PTR, PTR));
  private static final MethodHandle ATTRIBUTES =
      h("aster_dispatch_attributes", FunctionDescriptor.of(I32, I64, I64, I64, PTR, PTR, PTR));

  // ── builder ──────────────────────────────────────────────────────────────

  /** {@code aster_server_builder_new} … {@code aster_server_start}. */
  public static final class Builder implements AutoCloseable {
    private final long runtimeHandle;
    private long handle;

    public Builder(long runtimeHandle, long nodeHandle) {
      this.runtimeHandle = runtimeHandle;
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment out = arena.allocate(I64);
        check((int) BUILDER_NEW.invoke(runtimeHandle, nodeHandle, out), "aster_server_builder_new");
        this.handle = out.get(I64, 0);
      } catch (Throwable t) {
        throw rethrow(t);
      }
    }

    /** Register one service (design §1.5 JSON); returns its {@code service_idx}. */
    public int addServiceJson(String json) {
      byte[] bytes = json.getBytes(StandardCharsets.UTF_8);
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment seg = arena.allocateFrom(U8, bytes);
        MemorySegment idx = arena.allocate(I32);
        check(
            (int) ADD_SERVICE_JSON.invoke(handle, seg, bytes.length, idx),
            "aster_server_add_service_json");
        return idx.get(I32, 0);
      } catch (Throwable t) {
        throw rethrow(t);
      }
    }

    /** {@code aster_server_set_option} ({@code OPTION_*}). */
    public Builder option(int key, long value) {
      try {
        check((int) SET_OPTION.invoke(handle, key, value), "aster_server_set_option");
        return this;
      } catch (Throwable t) {
        throw rethrow(t);
      }
    }

    /** Start serving; consumes the builder. */
    public Dispatch start(int ringCapacity) {
      long h = handle;
      handle = 0;
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment out = arena.allocate(I64);
        check((int) START.invoke(runtimeHandle, h, ringCapacity, out), "aster_server_start");
        return new Dispatch(runtimeHandle, out.get(I64, 0));
      } catch (Throwable t) {
        throw rethrow(t);
      }
    }

    @Override
    public void close() {
      if (handle != 0) {
        try {
          BUILDER_FREE.invoke(handle);
        } catch (Throwable ignored) {
          // best effort
        }
        handle = 0;
      }
    }
  }

  /** {@code contract_id} of a service's JSON registration form, as Rust computes it. */
  public static String contractId(String json) {
    byte[] bytes = json.getBytes(StandardCharsets.UTF_8);
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment seg = arena.allocateFrom(U8, bytes);
      MemorySegment out = arena.allocate(16, 8);
      check((int) CONTRACT_ID.invoke(seg, bytes.length, out), "aster_dynamic_service_contract_id");
      MemorySegment ptr = out.get(PTR, 0);
      long len = out.get(I64, 8);
      String id = new String(ptr.reinterpret(len).toArray(U8), StandardCharsets.UTF_8);
      STRING_RELEASE.invoke(ptr, len);
      return id;
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  // ── instance ─────────────────────────────────────────────────────────────

  private final long runtimeHandle;
  private final long handle;
  private final ConcurrentHashMap<Long, CompletableFuture<Void>> capacityWaiters =
      new ConcurrentHashMap<>();
  private volatile boolean closed;

  private Dispatch(long runtimeHandle, long handle) {
    this.runtimeHandle = runtimeHandle;
    this.handle = handle;
  }

  public long handle() {
    return handle;
  }

  /**
   * Drain up to {@code max} events into {@code out} ({@code max * EVENT_LAYOUT.byteSize()} bytes).
   * {@code timeoutMs} 0 returns at once. Single consumer.
   */
  public int poll(MemorySegment out, int max, int timeoutMs) {
    try {
      return (int) POLL.invoke(runtimeHandle, handle, out, max, timeoutMs);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /** Release an event's (or a recv / metadata) lease. 0 is a no-op. */
  public void bufferRelease(long buffer) {
    if (buffer == 0) {
      return;
    }
    try {
      BUFFER_RELEASE.invoke(runtimeHandle, handle, buffer);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /** Live calls in the server's call table (diagnostics / tests). */
  public long liveCalls() {
    try {
      return (long) LIVE_CALLS.invoke(handle);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /** Outcome of a terminal or data operation. */
  public enum Outcome {
    /** Queued. */
    OK,
    /** The call is over (finished, cancelled, deadline, connection gone): nothing was sent. */
    GONE
  }

  /**
   * Terminal single-shot response (unary / client-stream): {@code payload} plus the trailer. {@code
   * statusAev1} null = OK. Parks while the queue is full.
   */
  public Outcome respond(long callId, byte[] payload, byte[] statusAev1) {
    MemorySegment p = heap(payload);
    int plen = payload == null ? 0 : payload.length;
    MemorySegment s = heap(statusAev1);
    int slen = statusAev1 == null ? 0 : statusAev1.length;
    return submit(
        callId,
        "aster_dispatch_respond",
        req -> (int) RESPOND.invoke(runtimeHandle, handle, callId, p, plen, s, slen, req));
  }

  /** One response data frame (server-stream / bidi); {@code payload} must not be empty. */
  public Outcome send(long callId, byte[] payload) {
    MemorySegment p = heap(payload);
    int plen = payload.length;
    return submit(
        callId,
        "aster_dispatch_send",
        req -> (int) SEND.invoke(runtimeHandle, handle, callId, p, plen, req));
  }

  /** The trailer. {@code statusAev1} null = OK. */
  public Outcome finish(long callId, byte[] statusAev1) {
    MemorySegment s = heap(statusAev1);
    int slen = statusAev1 == null ? 0 : statusAev1.length;
    return submit(
        callId,
        "aster_dispatch_finish",
        req -> (int) FINISH.invoke(runtimeHandle, handle, callId, s, slen, req));
  }

  /** End the call with {@code INTERNAL "handler dropped call"} unless it already ended. */
  public void abandon(long callId) {
    try {
      ABANDON.invoke(runtimeHandle, handle, callId);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /** Whether the call was cancelled, finished, or is unknown (stop producing). */
  public boolean isCancelled(long callId) {
    try {
      return (int) IS_CANCELLED.invoke(runtimeHandle, handle, callId) != 0;
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * The next request payload after the first (client-stream / bidi); {@code null} at the end of the
   * request stream or when the call is over. Blocks in bounded {@link #RECV_SLICE_MS} native waits:
   * between them it throws {@link java.util.concurrent.CancellationException} if the call was
   * cancelled (peer cancel, deadline, server stopping) or the thread was interrupted (the interrupt
   * flag is kept). Run it off virtual threads.
   */
  public byte[] recv(long callId) {
    while (true) {
      byte[] next = recvSlice(callId, RECV_SLICE_MS);
      if (next != TIMED_OUT) {
        return next;
      }
      if (Thread.currentThread().isInterrupted()) {
        throw new java.util.concurrent.CancellationException(
            "call " + callId + ": handler thread interrupted");
      }
      if (isCancelled(callId)) {
        throw new java.util.concurrent.CancellationException("call " + callId + " was cancelled");
      }
    }
  }

  /** Sentinel for a {@link #recvSlice} that timed out. */
  private static final byte[] TIMED_OUT = new byte[0];

  /**
   * One {@code aster_dispatch_recv} with a finite timeout: the payload, {@code null} at end of
   * stream / call over, or {@link #TIMED_OUT}.
   */
  private byte[] recvSlice(long callId, int timeoutMs) {
    try {
      MemorySegment out = RECV_SCRATCH.get();
      MemorySegment ptr = out.asSlice(0, 8);
      MemorySegment len = out.asSlice(8, 4);
      MemorySegment buf = out.asSlice(16, 8);
      int status = (int) RECV.invoke(runtimeHandle, handle, callId, timeoutMs, ptr, len, buf);
      if (status == IrohStatus.TIMEOUT.code) {
        return TIMED_OUT;
      }
      if (status == IrohStatus.NOT_FOUND.code) {
        return null;
      }
      check(status, "aster_dispatch_recv");
      long buffer = buf.get(I64, 0);
      if (buffer == 0) {
        return null;
      }
      int n = len.get(I32, 0);
      byte[] bytes = n == 0 ? new byte[0] : ptr.get(PTR, 0).reinterpret(n).toArray(U8);
      bufferRelease(buffer);
      return bytes;
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /** Per-thread out-parameters for {@link #recv} (handlers run on pooled platform threads). */
  private static final ThreadLocal<MemorySegment> RECV_SCRATCH =
      ThreadLocal.withInitial(() -> Arena.ofAuto().allocate(32, 8));

  /** The call header's metadata; empty when the call is over or has none. */
  public Map<String, String> metadata(long callId) {
    return kvOut(METADATA, callId, "aster_dispatch_metadata");
  }

  /** The caller's attributes as Gate 3 saw them. */
  public Map<String, String> attributes(long callId) {
    return kvOut(ATTRIBUTES, callId, "aster_dispatch_attributes");
  }

  private Map<String, String> kvOut(MethodHandle fn, long callId, String op) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment ptr = arena.allocate(PTR);
      MemorySegment len = arena.allocate(I32);
      MemorySegment buf = arena.allocate(I64);
      int status = (int) fn.invoke(runtimeHandle, handle, callId, ptr, len, buf);
      if (status == IrohStatus.NOT_FOUND.code) {
        return Map.of();
      }
      check(status, op);
      int n = len.get(I32, 0);
      byte[] bytes = n == 0 ? new byte[0] : ptr.get(PTR, 0).reinterpret(n).toArray(U8);
      bufferRelease(buf.get(I64, 0));
      return decodeKv(bytes);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * Attribute bridge (design §2.6): the attributes Gate 3 sees for {@code peer}. Empty or null
   * clears them.
   */
  public void setPeerAttributes(String peer, Map<String, String> attributes) {
    byte[] p = peer.getBytes(StandardCharsets.UTF_8);
    byte[] kv = attributes == null || attributes.isEmpty() ? new byte[0] : encodeKv(attributes);
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment ps = arena.allocateFrom(U8, p);
      MemorySegment ks = kv.length == 0 ? MemorySegment.NULL : arena.allocateFrom(U8, kv);
      check(
          (int) SET_PEER_ATTRIBUTES.invoke(handle, ps, p.length, ks, kv.length),
          "aster_server_set_peer_attributes");
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  // ── QUEUE_FULL → reserve → park ──────────────────────────────────────────

  /** One native submission attempt; writes the required length on {@code QUEUE_FULL}. */
  @FunctionalInterface
  interface Attempt {
    int invoke(MemorySegment requiredLenOut) throws Throwable;
  }

  /** Wait until native code holds response capacity for the exact retry. */
  @FunctionalInterface
  interface CapacityWait {
    void await(long requiredLen) throws Throwable;
  }

  private Outcome submit(long callId, String op, Attempt attempt) {
    try {
      return retryQueueFull(op, attempt, required -> awaitCapacity(callId, required, op));
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * Run {@code attempt} until it is not {@code QUEUE_FULL}, waiting for capacity between tries.
   * Package-private so the state machine has a deterministic unit test.
   *
   * <p>The hot path passes no out-pointer (nothing to allocate); only after a {@code QUEUE_FULL}
   * does the retry pass one to learn the required length.
   */
  static Outcome retryQueueFull(String op, Attempt attempt, CapacityWait wait) throws Throwable {
    long[] required = new long[1];
    MemorySegment out = MemorySegment.NULL;
    while (true) {
      int status = attempt.invoke(out);
      if (status == IrohStatus.QUEUE_FULL.code) {
        QUEUE_FULL_HITS.increment();
        if (out == MemorySegment.NULL) {
          // Learn the exact length first (the retry may also just succeed).
          out = MemorySegment.ofArray(required);
          continue;
        }
        wait.await(required[0]);
        continue;
      }
      if (status == IrohStatus.OK.code) {
        return Outcome.OK;
      }
      if (status == IrohStatus.NOT_FOUND.code || status == IrohStatus.ALREADY_CLOSED.code) {
        return Outcome.GONE;
      }
      check(status, op);
    }
  }

  private void awaitCapacity(long callId, long requiredLen, String op) throws Throwable {
    CompletableFuture<Void> waiter = new CompletableFuture<>();
    if (capacityWaiters.putIfAbsent(callId, waiter) != null) {
      throw new IllegalStateException(
          "concurrent response-capacity waits violate the one-producer contract for call "
              + callId);
    }
    int status;
    try {
      status = (int) RESERVE.invoke(runtimeHandle, handle, callId, requiredLen);
    } catch (Throwable t) {
      capacityWaiters.remove(callId, waiter);
      throw t;
    }
    if (status != IrohStatus.OK.code) {
      capacityWaiters.remove(callId, waiter);
      if (status == IrohStatus.NOT_FOUND.code || status == IrohStatus.ALREADY_CLOSED.code) {
        return; // the retry reports GONE
      }
      check(status, "aster_dispatch_reserve");
    }
    try {
      waiter.get();
    } catch (InterruptedException e) {
      capacityWaiters.remove(callId, waiter);
      CANCEL_RESERVATION.invoke(runtimeHandle, handle, callId);
      Thread.currentThread().interrupt();
      throw IrohException.forStatus(
          IrohStatus.CANCELLED, op + " interrupted while the response queue was full");
    } catch (ExecutionException e) {
      // RESPONSE_CLOSED: the call ended; the retry reports GONE.
    }
  }

  /** Complete a capacity waiter from the poll thread ({@code RESPONSE_WRITABLE/CLOSED}). */
  public void handleCapacityEvent(byte kind, long callId) {
    CompletableFuture<Void> waiter = capacityWaiters.remove(callId);
    if (waiter == null) {
      return;
    }
    if (kind == EVENT_RESPONSE_WRITABLE) {
      waiter.complete(null);
    } else {
      waiter.completeExceptionally(
          IrohException.forStatus(IrohStatus.NOT_FOUND, "response closed for call " + callId));
    }
  }

  private static final LongAdder QUEUE_FULL_HITS = new LongAdder();

  /** Diagnostic: total {@code QUEUE_FULL} results observed in this JVM. */
  public static long queueFullHits() {
    return QUEUE_FULL_HITS.sum();
  }

  /** Reset the diagnostic counter (benchmarks / tests). */
  public static void resetQueueFullStats() {
    QUEUE_FULL_HITS.reset();
  }

  @Override
  public void close() {
    if (closed) {
      return;
    }
    closed = true;
    IrohException closedError =
        IrohException.forStatus(IrohStatus.NOT_FOUND, "server closed during response wait");
    capacityWaiters.forEach((id, w) -> w.completeExceptionally(closedError));
    capacityWaiters.clear();
    try {
      DESTROY.invoke(runtimeHandle, handle);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  // ── k/v lists ────────────────────────────────────────────────────────────

  /**
   * The C ABI k/v list: {@code u32 LE count}, then per entry {@code u32 LE len} + key, {@code u32
   * LE len} + value (UTF-8).
   */
  public static byte[] encodeKv(Map<String, String> map) {
    int size = 4;
    byte[][] parts = new byte[map.size() * 2][];
    int i = 0;
    for (Map.Entry<String, String> e : map.entrySet()) {
      parts[i] = e.getKey().getBytes(StandardCharsets.UTF_8);
      parts[i + 1] = (e.getValue() == null ? "" : e.getValue()).getBytes(StandardCharsets.UTF_8);
      size += 8 + parts[i].length + parts[i + 1].length;
      i += 2;
    }
    ByteBuffer b = ByteBuffer.allocate(size).order(ByteOrder.LITTLE_ENDIAN);
    b.putInt(map.size());
    for (byte[] part : parts) {
      b.putInt(part.length);
      b.put(part);
    }
    return b.array();
  }

  /** Decode a k/v list (insertion order kept). */
  public static Map<String, String> decodeKv(byte[] bytes) {
    if (bytes.length == 0) {
      return Map.of();
    }
    ByteBuffer b = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
    int n = b.getInt();
    Map<String, String> out = new LinkedHashMap<>();
    for (int i = 0; i < n; i++) {
      byte[] k = new byte[b.getInt()];
      b.get(k);
      byte[] v = new byte[b.getInt()];
      b.get(v);
      out.put(new String(k, StandardCharsets.UTF_8), new String(v, StandardCharsets.UTF_8));
    }
    return out;
  }

  // ── helpers ──────────────────────────────────────────────────────────────

  private static MemorySegment heap(byte[] bytes) {
    return bytes == null || bytes.length == 0 ? MemorySegment.NULL : MemorySegment.ofArray(bytes);
  }

  static void check(int code, String op) {
    if (code != IrohStatus.OK.code) {
      IrohStatus status = IrohStatus.fromCode(code);
      throw IrohException.forStatus(status, op + " failed: " + status.name() + lastError());
    }
  }

  private static String lastError() {
    String msg = DataPlane.lastError();
    return msg == null || msg.isEmpty() ? "" : " (" + msg + ")";
  }

  static RuntimeException rethrow(Throwable t) {
    if (t instanceof RuntimeException re) {
      return re;
    }
    if (t instanceof Error err) {
      throw err;
    }
    return new RuntimeException(t);
  }
}
