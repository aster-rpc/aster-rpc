package site.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Java FFM wrapper for the payload-level RPC client of C ABI 3.5: {@code aster_rpc_unary}, {@code
 * aster_rpc_open}, {@code aster_call_send_payload} / {@code _recv_payload} / {@code _cancel}, over
 * Rust's pooled {@code RpcConnection} (design §3, §4.4).
 *
 * <p>Rust builds the {@code StreamHeader}, frames, compresses, enforces the deadline and decodes
 * trailers; Java passes {@link Options} and request payloads and gets response payloads plus a
 * status code (and AEv1 {@code RpcStatus} bytes for failures). Every function here may block (a
 * network round trip), so callers run them on platform threads.
 */
public final class Rpc {

  private Rpc() {}

  /** {@code aster_call_options_t} (72 bytes, ffi/src/layout.rs). */
  public static final MemoryLayout OPTIONS_LAYOUT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), //        0
          ValueLayout.JAVA_INT.withName("version"), //            4
          ValueLayout.ADDRESS.withName("service_ptr"), //         8
          ValueLayout.JAVA_LONG.withName("service_len"), //      16
          ValueLayout.ADDRESS.withName("method_ptr"), //         24
          ValueLayout.JAVA_LONG.withName("method_len"), //       32
          ValueLayout.ADDRESS.withName("metadata_ptr"), //       40
          ValueLayout.JAVA_LONG.withName("metadata_len"), //     48
          ValueLayout.JAVA_INT.withName("session_id"), //        56
          ValueLayout.JAVA_INT.withName("deadline_ms"), //       60
          ValueLayout.JAVA_INT.withName("serialization_mode"), // 64
          ValueLayout.JAVA_INT.withName("wire_call_id") //       68
          );

  /** {@code aster_method_pattern_t} values for {@link #open}. */
  public static final int PATTERN_SERVER_STREAM = 1;

  public static final int PATTERN_CLIENT_STREAM = 2;
  public static final int PATTERN_BIDI_STREAM = 3;

  /**
   * Slice for {@link #recvPolling}: native reads never wait longer than this before the binding
   * re-checks its own cancellation (no {@code ASTER_TIMEOUT_INFINITE} waits).
   */
  public static final int POLL_SLICE_MS = 200;

  /** After a binding-side cancel, how long to wait for the trailer before synthesizing one. */
  static final long CANCEL_GRACE_MS = 5_000;

  private static final ValueLayout.OfInt I32 = ValueLayout.JAVA_INT;
  private static final ValueLayout.OfLong I64 = ValueLayout.JAVA_LONG;
  private static final ValueLayout.OfByte U8 = ValueLayout.JAVA_BYTE;
  private static final java.lang.foreign.AddressLayout PTR = ValueLayout.ADDRESS;

  private static MethodHandle h(String name, FunctionDescriptor d) {
    return IrohLibrary.getInstance().getHandle(name, d);
  }

  private static final MethodHandle UNARY =
      h(
          "aster_rpc_unary",
          FunctionDescriptor.of(I32, I64, I64, PTR, PTR, I32, PTR, PTR, PTR, PTR, PTR, PTR, PTR));
  private static final MethodHandle UNARY_CANCELLABLE =
      h(
          "aster_rpc_unary_cancellable",
          FunctionDescriptor.of(
              I32, I64, I64, PTR, I64, PTR, I32, PTR, PTR, PTR, PTR, PTR, PTR, PTR));
  // Token calls never block: no thread-state transition (critical).
  private static final MethodHandle CANCEL_TOKEN_NEW =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_cancel_token_new",
              FunctionDescriptor.of(I32, I64, PTR),
              Linker.Option.critical(false));
  private static final MethodHandle CANCEL_TOKEN_CANCEL =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_cancel_token_cancel",
              FunctionDescriptor.of(I32, I64, I64),
              Linker.Option.critical(false));
  private static final MethodHandle CANCEL_TOKEN_RELEASE =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_cancel_token_release",
              FunctionDescriptor.of(I32, I64, I64),
              Linker.Option.critical(false));
  private static final MethodHandle OPEN =
      h("aster_rpc_open", FunctionDescriptor.of(I32, I64, I64, U8, PTR, PTR));
  private static final MethodHandle SEND =
      h("aster_call_send_payload", FunctionDescriptor.of(I32, I64, I64, PTR, I32, U8));
  private static final MethodHandle SEND_MANY =
      h("aster_call_send_payloads", FunctionDescriptor.of(I32, I64, I64, PTR, I32, U8));
  private static final MethodHandle RECV =
      h("aster_call_recv_payload", FunctionDescriptor.of(I32, I64, I64, I32, PTR, PTR, PTR, PTR));
  private static final MethodHandle CANCEL =
      h("aster_call_cancel", FunctionDescriptor.of(I32, I64, I64));
  private static final MethodHandle RELEASE =
      h("aster_call_release", FunctionDescriptor.of(I32, I64, I64));
  private static final MethodHandle NEXT_SESSION_ID =
      h("aster_connection_next_session_id", FunctionDescriptor.of(I32, I64, I64, PTR));
  private static final MethodHandle BUFFER_RELEASE =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_call_buffer_release",
              FunctionDescriptor.of(I32, I64, I64),
              Linker.Option.critical(false));

  /** Per-call options (an {@code aster_call_options_t}). */
  public record Options(
      String service,
      String method,
      int version,
      int sessionId,
      int deadlineMs,
      Map<String, String> metadata) {

    public static Options of(String service, String method, int sessionId) {
      return new Options(service, method, 1, sessionId, 0, Map.of());
    }

    boolean cacheable() {
      return sessionId == 0 && deadlineMs == 0 && (metadata == null || metadata.isEmpty());
    }
  }

  /** Native options for the common (shared, no deadline, no metadata) calls, built once. */
  private static final Map<String, MemorySegment> OPTIONS_CACHE = new ConcurrentHashMap<>();

  /**
   * {@code o} with the current trace context (call-context carrier) as {@code traceparent} / {@code
   * baggage} metadata; Rust parents its client span on the {@code traceparent} (design §5.4). The
   * same object when there is no context.
   */
  private static Options traced(Options o) {
    Map<String, String> injected = site.aster.otel.Tracing.inject(o.metadata());
    return injected == o.metadata()
        ? o
        : new Options(
            o.service(), o.method(), o.version(), o.sessionId(), o.deadlineMs(), injected);
  }

  private static MemorySegment options(Options o, Arena arena) {
    if (o.cacheable()) {
      String key = o.service() + '\u0000' + o.method() + '\u0000' + o.version();
      MemorySegment cached = OPTIONS_CACHE.get(key);
      if (cached != null) {
        return cached;
      }
      MemorySegment built = buildOptions(o, Arena.global());
      MemorySegment prior = OPTIONS_CACHE.putIfAbsent(key, built);
      return prior != null ? prior : built;
    }
    return buildOptions(o, arena);
  }

  private static MemorySegment buildOptions(Options o, Arena arena) {
    MemorySegment s = arena.allocate(OPTIONS_LAYOUT);
    byte[] service = o.service().getBytes(StandardCharsets.UTF_8);
    byte[] method = o.method().getBytes(StandardCharsets.UTF_8);
    s.set(I32, 0, (int) OPTIONS_LAYOUT.byteSize());
    s.set(I32, 4, o.version());
    s.set(PTR, 8, arena.allocateFrom(U8, service));
    s.set(I64, 16, service.length);
    s.set(PTR, 24, arena.allocateFrom(U8, method));
    s.set(I64, 32, method.length);
    if (o.metadata() != null && !o.metadata().isEmpty()) {
      byte[] kv = Dispatch.encodeKv(o.metadata());
      s.set(PTR, 40, arena.allocateFrom(U8, kv));
      s.set(I64, 48, kv.length);
    }
    s.set(I32, 56, o.sessionId());
    s.set(I32, 60, o.deadlineMs());
    return s;
  }

  /**
   * Outcome of a unary call: {@code code == 0} with the response payload, or a failure code with
   * its AEv1 {@code RpcStatus} bytes.
   */
  public record UnaryResult(int code, byte[] payload, byte[] status) {}

  /** Per-thread native scratch: unary out-parameters and small requests (no arena per call). */
  private static final class Scratch {
    final MemorySegment out = Arena.ofAuto().allocate(64, 8);
    MemorySegment req = Arena.ofAuto().allocate(4096, 8);

    MemorySegment request(byte[] request) {
      if (request.length > req.byteSize()) {
        req = Arena.ofAuto().allocate(Math.max(request.length, req.byteSize() * 2), 8);
      }
      MemorySegment.copy(request, 0, req, U8, 0, request.length);
      return req;
    }
  }

  private static final int SCRATCH_REQUEST_LIMIT = 64 * 1024;

  private static final ThreadLocal<Scratch> SCRATCH = ThreadLocal.withInitial(Scratch::new);

  /** One unary call on the connection's pooled streams. Blocks for the round trip. */
  public static UnaryResult unary(long runtime, long connection, Options o, byte[] request) {
    return unary(runtime, connection, o, request, 0L);
  }

  /**
   * {@link #unary} that {@link #cancelToken cancelToken(token)} aborts ({@code
   * aster_rpc_unary_cancellable}, ABI 4.5): the same pooled single-write path. A cancelled call
   * returns {@code CANCELLED} and the server's handler sees the cancel. The call consumes {@code
   * token} ({@code 0} = not cancellable).
   */
  public static UnaryResult unary(
      long runtime, long connection, Options o, byte[] request, long token) {
    o = traced(o);
    boolean small = request.length <= SCRATCH_REQUEST_LIMIT;
    if (o.cacheable() && small) {
      return unaryIn(runtime, connection, o, request, null, token);
    }
    try (Arena arena = Arena.ofConfined()) {
      return unaryIn(runtime, connection, o, request, arena, token);
    }
  }

  /** A cancel token for one {@link #unary(long, long, Options, byte[], long)} call. */
  public static long newCancelToken(long runtime) {
    try {
      MemorySegment out = SCRATCH.get().out.asSlice(56, 8);
      Dispatch.check((int) CANCEL_TOKEN_NEW.invoke(runtime, out), "aster_cancel_token_new");
      return out.get(I64, 0);
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }

  /**
   * Cancel the call {@code token} was (or will be) passed to: any thread, never blocks, idempotent,
   * a no-op once the call has returned.
   */
  public static void cancelToken(long runtime, long token) {
    try {
      int ignored = (int) CANCEL_TOKEN_CANCEL.invoke(runtime, token); // NOT_FOUND: call over
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }

  /** Free a token that was never passed to a call (a call frees its own). */
  public static void releaseCancelToken(long runtime, long token) {
    try {
      int ignored = (int) CANCEL_TOKEN_RELEASE.invoke(runtime, token);
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }

  private static UnaryResult unaryIn(
      long runtime, long connection, Options o, byte[] request, Arena arena, long token) {
    try {
      Scratch scratch = SCRATCH.get();
      MemorySegment opts = options(o, arena);
      MemorySegment req;
      if (request.length == 0) {
        req = MemorySegment.NULL;
      } else if (request.length <= SCRATCH_REQUEST_LIMIT) {
        req = scratch.request(request);
      } else {
        req = arena.allocateFrom(U8, request);
      }
      MemorySegment out = scratch.out;
      MemorySegment respPtr = out.asSlice(0, 8);
      MemorySegment respLen = out.asSlice(8, 4);
      MemorySegment respBuf = out.asSlice(16, 8);
      MemorySegment code = out.asSlice(24, 4);
      MemorySegment stPtr = out.asSlice(32, 8);
      MemorySegment stLen = out.asSlice(40, 4);
      MemorySegment stBuf = out.asSlice(48, 8);
      int status =
          token == 0
              ? (int)
                  UNARY.invoke(
                      runtime,
                      connection,
                      opts,
                      req,
                      request.length,
                      respPtr,
                      respLen,
                      respBuf,
                      code,
                      stPtr,
                      stLen,
                      stBuf)
              : (int)
                  UNARY_CANCELLABLE.invoke(
                      runtime,
                      connection,
                      opts,
                      token,
                      req,
                      request.length,
                      respPtr,
                      respLen,
                      respBuf,
                      code,
                      stPtr,
                      stLen,
                      stBuf);
      Dispatch.check(status, token == 0 ? "aster_rpc_unary" : "aster_rpc_unary_cancellable");
      byte[] payload = take(runtime, respPtr, respLen, respBuf);
      byte[] st = take(runtime, stPtr, stLen, stBuf);
      return new UnaryResult(code.get(I32, 0), payload, st);
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }

  private static byte[] take(long runtime, MemorySegment ptr, MemorySegment len, MemorySegment buf)
      throws Throwable {
    long buffer = buf.get(I64, 0);
    int n = len.get(I32, 0);
    byte[] out = n == 0 ? new byte[0] : ptr.get(PTR, 0).reinterpret(n).toArray(U8);
    if (buffer != 0) {
      BUFFER_RELEASE.invoke(runtime, buffer);
    }
    return out;
  }

  /** Open a streaming call ({@code PATTERN_*}); returns the call handle. */
  public static long open(long runtime, long connection, int pattern, Options o) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(I64);
      int status =
          (int) OPEN.invoke(runtime, connection, (byte) pattern, options(traced(o), arena), out);
      Dispatch.check(status, "aster_rpc_open");
      return out.get(I64, 0);
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }

  /**
   * Send one request payload ({@code end} ends the request stream; an empty payload with {@code
   * end} only ends it). Returns false when the call is over — {@link #recv} then yields the trailer
   * that says why.
   */
  public static boolean send(long runtime, long call, byte[] payload, boolean end) {
    try {
      MemorySegment p = payload.length == 0 ? MemorySegment.NULL : SCRATCH.get().request(payload);
      int status = (int) SEND.invoke(runtime, call, p, payload.length, (byte) (end ? 1 : 0));
      if (status == IrohStatus.ALREADY_CLOSED.code) {
        return false;
      }
      Dispatch.check(status, "aster_call_send_payload");
      return true;
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }

  /**
   * Send several request payloads in one native call, framed into as few writes as possible ({@code
   * end} ends the request stream with the last). Client-stream / bidi only. Returns false when the
   * call is over.
   */
  public static boolean sendMany(long runtime, long call, List<byte[]> payloads, boolean end) {
    try (Arena arena = Arena.ofConfined()) {
      int n = payloads.size();
      MemorySegment descs = arena.allocate(16L * Math.max(n, 1), 8);
      for (int i = 0; i < n; i++) {
        byte[] p = payloads.get(i);
        MemorySegment seg = p.length == 0 ? MemorySegment.NULL : arena.allocateFrom(U8, p);
        descs.set(PTR, 16L * i, seg);
        descs.set(I64, 16L * i + 8, p.length);
      }
      int status = (int) SEND_MANY.invoke(runtime, call, descs, n, (byte) (end ? 1 : 0));
      if (status == IrohStatus.ALREADY_CLOSED.code) {
        return false;
      }
      Dispatch.check(status, "aster_call_send_payloads");
      return true;
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }

  /** What {@link #recv} returned. */
  public sealed interface Recv permits Recv.Data, Recv.Trailer, Recv.Timeout {
    /** A response payload. */
    record Data(byte[] payload) implements Recv {}

    /** The call is over; {@code status} is its AEv1 {@code RpcStatus} (code 0 = success). */
    record Trailer(byte[] status) implements Recv {}

    /** Nothing within the timeout; the read resumes on the next call. */
    record Timeout() implements Recv {}
  }

  private static final Recv.Timeout TIMEOUT = new Recv.Timeout();

  /** The next response payload, or the trailer. {@code timeoutMs} 0 = do not wait. */
  public static Recv recv(long runtime, long call, int timeoutMs) {
    try {
      MemorySegment out = SCRATCH.get().out;
      MemorySegment ptr = out.asSlice(0, 8);
      MemorySegment len = out.asSlice(8, 4);
      MemorySegment buf = out.asSlice(16, 8);
      MemorySegment trailer = out.asSlice(24, 1);
      int status = (int) RECV.invoke(runtime, call, timeoutMs, ptr, len, buf, trailer);
      if (status == IrohStatus.TIMEOUT.code) {
        return TIMEOUT;
      }
      if (status == IrohStatus.ALREADY_CLOSED.code) {
        return new Recv.Trailer(new byte[0]);
      }
      Dispatch.check(status, "aster_call_recv_payload");
      boolean isTrailer = trailer.get(U8, 0) != 0;
      byte[] bytes = take(runtime, ptr, len, buf);
      return isTrailer ? new Recv.Trailer(bytes) : new Recv.Data(bytes);
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }

  /**
   * The next response payload or the trailer, read in bounded {@link #POLL_SLICE_MS} slices.
   * Between slices, if the current thread is interrupted or {@code stop} returns true, the call is
   * cancelled natively ({@code aster_call_cancel}) and reading continues until the {@code
   * CANCELLED} trailer arrives (synthesized if it does not within {@link #CANCEL_GRACE_MS}). The
   * thread's interrupt flag is left as found.
   */
  public static Recv recvPolling(long runtime, long call, java.util.function.BooleanSupplier stop) {
    long cancelledAt = 0;
    while (true) {
      Recv r = recv(runtime, call, POLL_SLICE_MS);
      if (!(r instanceof Recv.Timeout)) {
        return r;
      }
      if (cancelledAt == 0) {
        if (Thread.currentThread().isInterrupted() || stop.getAsBoolean()) {
          cancel(runtime, call);
          cancelledAt = System.nanoTime();
        }
      } else if (System.nanoTime() - cancelledAt > CANCEL_GRACE_MS * 1_000_000L) {
        return new Recv.Trailer(cancelledStatus());
      }
    }
  }

  /** AEv1 {@code RpcStatus{CANCELLED}} for a call the binding cancelled. */
  static byte[] cancelledStatus() {
    return AsterEnvelope.encodeRpcStatus(
        new site.aster.server.wire.RpcStatus(
            site.aster.server.wire.RpcStatus.CANCELLED,
            "cancelled by the caller",
            List.of(),
            List.of()));
  }

  /** Cancel: {@code FLAG_CANCEL} then a reset; a blocked {@link #recv} returns the trailer. */
  public static void cancel(long runtime, long call) {
    try {
      CANCEL.invoke(runtime, call);
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }

  /** Free the call (cancelling it if unfinished). Idempotent. */
  public static void release(long runtime, long call) {
    try {
      RELEASE.invoke(runtime, call);
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }

  /** A session id from the connection's shared counter (unique across every client on it). */
  public static int nextSessionId(long runtime, long connection) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(I32);
      Dispatch.check(
          (int) NEXT_SESSION_ID.invoke(runtime, connection, out),
          "aster_connection_next_session_id");
      return out.get(I32, 0);
    } catch (Throwable t) {
      throw Dispatch.rethrow(t);
    }
  }
}
