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

  /** The call's server span is exported: read it with {@link #traceContext}. */
  public static final int FLAG_HAS_TRACE = 0x400;

  /** The caller sent {@code baggage} / {@code tracestate}: read them with {@link #inboundTrace}. */
  public static final int FLAG_HAS_INBOUND_TRACE = 0x800;

  /**
   * ABI 7.5: an {@code Authenticator} resolved a principal other than the transport peer id. {@code
   * peer_ptr} is then that principal; the transport peer comes from {@link #identity}. Clear:
   * {@code peer_ptr} is both.
   */
  public static final int FLAG_HAS_PRINCIPAL = 0x1000;

  /**
   * ABI 7.7: a {@code CALL} a gateway forwarded on behalf of a user ({@link #FLAG_HAS_PRINCIPAL} is
   * set too: {@code peer_ptr} is {@code iss|sub}, the transport peer the gateway). Read the user
   * with {@link #onBehalfOf}.
   */
  public static final int FLAG_ON_BEHALF_OF = 0x2000;

  // ── aster_dispatch_identity_t (64 bytes, ABI 7.5) ───────────────────────

  public static final MemoryLayout IDENTITY_LAYOUT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), //               0
          ValueLayout.JAVA_INT.withName("flags"), //                     4
          ValueLayout.ADDRESS.withName("peer_ptr"), //                   8
          ValueLayout.JAVA_INT.withName("peer_len"), //                 16
          MemoryLayout.paddingLayout(4), //                             20
          ValueLayout.ADDRESS.withName("principal_ptr"), //             24
          ValueLayout.JAVA_INT.withName("principal_len"), //            32
          MemoryLayout.paddingLayout(4), //                             36
          ValueLayout.ADDRESS.withName("actor_ptr"), //                 40
          ValueLayout.JAVA_INT.withName("actor_len"), //                48
          ValueLayout.JAVA_INT.withName("reserved"), //                 52
          ValueLayout.JAVA_LONG.withName("buffer") //                   56
          );

  // ── aster_drain_report_t (64 bytes, ABI 7.2) ────────────────────────────

  public static final MemoryLayout DRAIN_REPORT_LAYOUT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_LONG.withName("calls_in_flight"), //          0
          ValueLayout.JAVA_LONG.withName("calls_completed"), //          8
          ValueLayout.JAVA_LONG.withName("calls_cancelled"), //         16
          ValueLayout.JAVA_LONG.withName("calls_refused"), //           24
          ValueLayout.JAVA_LONG.withName("connections_closed"), //      32
          ValueLayout.JAVA_LONG.withName("connections_refused"), //     40
          ValueLayout.JAVA_LONG.withName("elapsed_ms"), //              48
          ValueLayout.JAVA_INT.withName("deadline_reached"), //         56
          ValueLayout.JAVA_INT.withName("reserved") //                  60
          );

  /**
   * Who a live call is from (ABI 7.5): {@code peer} is the transport id (64-hex node id over iroh,
   * the remote address over HTTP / WebTransport); {@code principal} the {@code Authenticator}'s
   * principal, else the same as {@code peer}.
   */
  public record Identity(String peer, String principal) {}

  /** {@code aster_server_option_t}. */
  public static final int OPTION_BUILTIN_NODE_INFO = 1;

  public static final int OPTION_BUILTIN_TOPOLOGY = 2;
  public static final int OPTION_MAX_SESSIONS_PER_CONNECTION = 3;
  public static final int OPTION_HANDLER_TIMEOUT_MS = 4;

  /**
   * ABI 7.6: accept calls carrying {@code aster-on-behalf-of} (0/1); overrides {@code [identity]
   * accept_on_behalf_of}.
   */
  public static final int OPTION_ACCEPT_ON_BEHALF_OF = 6;

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
  private static final MethodHandle SET_TOPOLOGY =
      h("aster_server_set_topology", FunctionDescriptor.of(I32, I64, I32, IrohLibrary.ASTER_BYTES));
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
  private static final MethodHandle RESTRICT_PEER_ROLES =
      h("aster_server_restrict_peer_roles", FunctionDescriptor.of(I32, I64, PTR, I32, PTR, I32));
  private static final MethodHandle CLEAR_PEER_RESTRICTION =
      h("aster_server_clear_peer_restriction", FunctionDescriptor.of(I32, I64, PTR, I32));
  private static final MethodHandle SET_NODE_ATTRIBUTE =
      h("aster_server_set_node_attribute", FunctionDescriptor.of(I32, I64, PTR, I32, PTR, I32));
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
  private static final MethodHandle INBOUND_TRACE =
      h("aster_dispatch_inbound_trace", FunctionDescriptor.of(I32, I64, I64, I64, PTR, PTR, PTR));
  private static final MethodHandle TRACE_CONTEXT =
      h("aster_dispatch_trace_context", FunctionDescriptor.of(I32, I64, I64, I64, PTR));
  private static final MethodHandle ADD_LEASE_GRANTOR =
      h(
          "aster_server_add_lease_grantor",
          FunctionDescriptor.of(I32, I64, I64, IrohLibrary.ASTER_BYTES, IrohLibrary.ASTER_BYTES));
  private static final MethodHandle ATTACH_HEALTH =
      h("aster_server_attach_health", FunctionDescriptor.of(I32, I64, I64));
  private static final MethodHandle IDENTITY =
      h("aster_dispatch_identity", FunctionDescriptor.of(I32, I64, I64, I64, PTR));
  private static final MethodHandle DRAIN =
      h("aster_server_drain", FunctionDescriptor.of(I32, I64, I64, I32, PTR));
  private static final MethodHandle ON_BEHALF_OF =
      h("aster_dispatch_on_behalf_of", FunctionDescriptor.of(I32, I64, I64, I64, PTR, PTR, PTR));
  private static final MethodHandle NODE_PROPERTIES =
      h("aster_server_node_properties", FunctionDescriptor.of(I32, I64, PTR));
  private static final MethodHandle NODE_CONFIG_GENERATION =
      h("aster_server_node_config_generation", FunctionDescriptor.of(I64, I64));
  private static final MethodHandle SET_AUDIT_OUTBOX =
      h("aster_server_set_audit_outbox", FunctionDescriptor.of(I32, I64, I64, I64));

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

    /**
     * Opt in to the native topology RPC baseline, with optional Rust-enforced Gate 3 roles.
     * Enabling requires monitoring. A null requirement admits any transport-admitted caller.
     */
    public Builder topology(
        boolean enabled, site.aster.contract.CapabilityRequirement requirement) {
      try (Arena arena = Arena.ofConfined()) {
        byte[] json =
            requirement == null
                ? new byte[0]
                : new com.fasterxml.jackson.databind.ObjectMapper().writeValueAsBytes(requirement);
        NativeCall.callChecked(
            SET_TOPOLOGY,
            "aster_server_set_topology",
            handle,
            enabled ? 1 : 0,
            NativeCall.bytes(arena, json));
        return this;
      } catch (java.io.IOException e) {
        throw new IllegalArgumentException(e);
      }
    }

    /**
     * {@code aster_server_attach_health}: the ops listener {@code healthHandle} (see {@code
     * site.aster.health.HealthServer}) counts this server's calls, is set ready by a successful
     * start and not ready first by {@link Dispatch#close}. Call before {@link #start}.
     */
    public Builder attachHealth(long healthHandle) {
      try {
        check((int) ATTACH_HEALTH.invoke(handle, healthHandle), "aster_server_attach_health");
        return this;
      } catch (Throwable t) {
        throw rethrow(t);
      }
    }

    /**
     * {@code aster_server_add_lease_grantor} (ABI 5.1): serve {@code aster.lease.Grantor} backed by
     * the lease serializer handle {@code serializer}. {@code requiresJson} gates every method
     * (empty = none); {@code revokeRequiresJson} gates revoke (empty = the {@code operator} role,
     * {@code null} JSON = only {@code requiresJson}).
     */
    public Builder leaseGrantor(long serializer, byte[] requiresJson, byte[] revokeRequiresJson) {
      try (Arena arena = Arena.ofConfined()) {
        NativeCall.callChecked(
            ADD_LEASE_GRANTOR,
            "aster_server_add_lease_grantor",
            handle,
            serializer,
            NativeCall.bytes(arena, requiresJson),
            NativeCall.bytes(arena, revokeRequiresJson));
        return this;
      }
    }

    /**
     * {@code aster_server_set_audit_outbox} (ABI 7.7): record every {@code aster.ops.NodeConfig}
     * set and clear in the durable audit outbox {@code outbox} (an {@code aster_audit_outbox_t}).
     * {@code UNSUPPORTED} when the config turns NodeConfig off.
     */
    public Builder auditOutbox(long outbox) {
      try {
        check(
            (int) SET_AUDIT_OUTBOX.invoke(runtimeHandle, handle, outbox),
            "aster_server_set_audit_outbox");
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
   * Bind an authenticated HTTPS/optional WebTransport child. Startup is serialized with server
   * destruction; the upcall table must belong to this runtime and serve kind-80 authentication.
   */
  public synchronized site.aster.server.HttpListener startHttp(
      site.aster.upcall.Upcalls upcalls, site.aster.server.HttpListenerConfig config) {
    if (closed) {
      throw IrohException.forStatus(IrohStatus.ALREADY_CLOSED, "server closed");
    }
    return site.aster.server.HttpListener.start(runtimeHandle, handle, upcalls, config);
  }

  /**
   * Explicit anonymous HTTPS/WebTransport; sessions are disabled and attribute gates remain
   * enforced.
   */
  public synchronized site.aster.server.HttpListener startPublicHttp(
      site.aster.server.HttpListenerConfig config) {
    if (closed) throw IrohException.forStatus(IrohStatus.ALREADY_CLOSED, "server closed");
    return site.aster.server.HttpListener.startPublic(runtimeHandle, handle, config);
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

  /**
   * The server span context of a live call ({@link #FLAG_HAS_TRACE}); {@code null} when the span is
   * not exported (an all-zero context) or the call is over.
   */
  public site.aster.otel.SpanContext traceContext(long callId) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(32, 8);
      int status = (int) TRACE_CONTEXT.invoke(runtimeHandle, handle, callId, out);
      if (status == IrohStatus.NOT_FOUND.code) {
        return null;
      }
      check(status, "aster_dispatch_trace_context");
      return site.aster.otel.SpanContext.fromNative(out);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * The inbound W3C {@code baggage} and {@code tracestate} of a live call ({@link
   * #FLAG_HAS_INBOUND_TRACE}), by those keys; empty when the call is over.
   */
  public Map<String, String> inboundTrace(long callId) {
    return kvOut(INBOUND_TRACE, callId, "aster_dispatch_inbound_trace");
  }

  /**
   * The transport peer and principal of a live call (ABI 7.5; only needed with {@link
   * #FLAG_HAS_PRINCIPAL}). {@code null} once the call is over ({@code NOT_FOUND}).
   */
  public Identity identity(long callId) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(IDENTITY_LAYOUT);
      out.set(I32, 0, (int) IDENTITY_LAYOUT.byteSize());
      int status = (int) IDENTITY.invoke(runtimeHandle, handle, callId, out);
      if (status == IrohStatus.NOT_FOUND.code) {
        return null;
      }
      check(status, "aster_dispatch_identity");
      String peer = utf8(out.get(PTR, 8), out.get(I32, 16));
      String principal = utf8(out.get(PTR, 24), out.get(I32, 32));
      bufferRelease(out.get(I64, 56));
      return new Identity(peer, principal.isEmpty() ? peer : principal);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * The forwarded identity of a live call ({@code aster_dispatch_on_behalf_of}, ABI 7.7; only for
   * {@link #FLAG_ON_BEHALF_OF}): {@code null} for a call that was not forwarded or is over.
   */
  public site.aster.identity.ForwardedIdentity onBehalfOf(long callId) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment ptr = arena.allocate(PTR);
      MemorySegment len = arena.allocate(I32);
      MemorySegment buf = arena.allocate(I64);
      int status = (int) ON_BEHALF_OF.invoke(runtimeHandle, handle, callId, ptr, len, buf);
      if (status == IrohStatus.NOT_FOUND.code) {
        return null;
      }
      check(status, "aster_dispatch_on_behalf_of");
      int n = len.get(I32, 0);
      byte[] bytes = n == 0 ? new byte[0] : ptr.get(PTR, 0).reinterpret(n).toArray(U8);
      bufferRelease(buf.get(I64, 0));
      if (bytes.length == 0) {
        return null;
      }
      // The list repeats amr, so it is read in order rather than as a map.
      ByteBuffer b = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
      int count = b.getInt();
      String iss = "";
      String sub = "";
      String jti = "";
      java.time.Instant exp = null;
      java.util.List<String> amr = new java.util.ArrayList<>();
      for (int i = 0; i < count; i++) {
        byte[] k = new byte[b.getInt()];
        b.get(k);
        byte[] v = new byte[b.getInt()];
        b.get(v);
        String value = new String(v, StandardCharsets.UTF_8);
        switch (new String(k, StandardCharsets.UTF_8)) {
          case "iss" -> iss = value;
          case "sub" -> sub = value;
          case "jti" -> jti = value;
          case "exp" -> {
            try {
              exp = java.time.Instant.ofEpochSecond(Long.parseLong(value));
            } catch (NumberFormatException ignored) {
              // left unset
            }
          }
          case "amr" -> amr.add(value);
          default -> {
            // a later minor may add keys
          }
        }
      }
      return new site.aster.identity.ForwardedIdentity(iss, sub, jti, exp, amr);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * {@code aster_server_node_config_generation} (ABI 7.7): 0 = never changed, or NodeConfig off.
   */
  public long nodeConfigGeneration() {
    try {
      return (long) NODE_CONFIG_GENERATION.invoke(handle);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /** {@code aster_server_node_properties} (ABI 7.7) as JSON bytes. */
  public byte[] nodePropertiesJson() {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(16, 8);
      check((int) NODE_PROPERTIES.invoke(handle, out), "aster_server_node_properties");
      MemorySegment ptr = out.get(PTR, 0);
      long len = out.get(I64, 8);
      if (ptr.equals(MemorySegment.NULL) || len == 0) {
        return new byte[0];
      }
      byte[] json = ptr.reinterpret(len).toArray(U8);
      STRING_RELEASE.invoke(ptr, len);
      return json;
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  private static String utf8(MemorySegment ptr, int len) {
    if (len == 0 || ptr.equals(MemorySegment.NULL)) {
      return "";
    }
    return new String(ptr.reinterpret(len).toArray(U8), StandardCharsets.UTF_8);
  }

  /**
   * {@code aster_server_drain} (ABI 7.2): blocks until the drain ends (see {@link
   * site.aster.server.AsterServer#drain}). A concurrent call with a shorter grace cuts a running
   * drain short and returns the same report. Must not run on a tokio thread.
   */
  public site.aster.server.DrainReport drain(long graceMs) {
    int grace = (int) Math.min(Math.max(graceMs, 0L), 0xFFFF_FFFFL);
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(DRAIN_REPORT_LAYOUT);
      check((int) DRAIN.invoke(runtimeHandle, handle, grace, out), "aster_server_drain");
      return new site.aster.server.DrainReport(
          out.get(I64, 0),
          out.get(I64, 8),
          out.get(I64, 16),
          out.get(I64, 24),
          out.get(I64, 32),
          out.get(I64, 40),
          out.get(I32, 56) != 0,
          java.time.Duration.ofMillis(out.get(I64, 48)));
    } catch (Throwable t) {
      throw rethrow(t);
    }
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

  /**
   * Narrow an enrolled {@code peer}'s roles to {@code roles} (comma-separated) intersected with its
   * credential roles. {@code NOT_FOUND} without a live enrollment session.
   */
  public void restrictPeerRoles(String peer, String roles) {
    byte[] p = peer.getBytes(StandardCharsets.UTF_8);
    byte[] r = roles.getBytes(StandardCharsets.UTF_8);
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment ps = arena.allocateFrom(U8, p);
      MemorySegment rs = r.length == 0 ? MemorySegment.NULL : arena.allocateFrom(U8, r);
      check(
          (int) RESTRICT_PEER_ROLES.invoke(handle, ps, p.length, rs, r.length),
          "aster_server_restrict_peer_roles");
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /** Remove {@code peer}'s role restriction. */
  public void clearPeerRestriction(String peer) {
    byte[] p = peer.getBytes(StandardCharsets.UTF_8);
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment ps = arena.allocateFrom(U8, p);
      check(
          (int) CLEAR_PEER_RESTRICTION.invoke(handle, ps, p.length),
          "aster_server_clear_peer_restriction");
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * Set (or, with an empty value, remove) a text attribute of the served {@code aster.ops.NodeInfo}
   * identity (ABI 5.1), e.g. {@code aster.registry_namespace}.
   */
  public void setNodeAttribute(String name, String value) {
    byte[] n = name.getBytes(StandardCharsets.UTF_8);
    byte[] v = value == null ? new byte[0] : value.getBytes(StandardCharsets.UTF_8);
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment ns = arena.allocateFrom(U8, n);
      MemorySegment vs = v.length == 0 ? MemorySegment.NULL : arena.allocateFrom(U8, v);
      check(
          (int) SET_NODE_ATTRIBUTE.invoke(handle, ns, n.length, vs, v.length),
          "aster_server_set_node_attribute");
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
  public synchronized void close() {
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
