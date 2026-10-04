package site.aster.upcall;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.FutureTask;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;
import site.aster.handle.IrohRuntime;
import site.aster.lease.FencedResource;
import site.aster.lease.LeaseAuthorizer;
import site.aster.lease.LeaseSerializer;
import site.aster.lease.MutateResult;

/**
 * One upcall table per runtime (C ABI 4.1, design {@code docs/_internal/bindings/upcall-design.md}
 * §4): Rust asks Java a question as an event, Java answers by invocation id.
 *
 * <p>A poll loop on a <b>virtual thread</b> drains {@code aster_upcall_poll} in batches and runs
 * each upcall's handler on its own virtual thread, so a slow handler holds nothing else up and
 * answers may come in any order. The loop never blocks in native code (an FFM downcall would pin
 * its carrier): it polls without waiting and parks between empty polls with a short back-off (at
 * most {@link #MAX_IDLE_PARK}), which bounds the added latency on an idle table.
 *
 * <p>Handlers are registered per kind ({@link #serveLeaseSerializer}, {@link
 * #serveLeaseAuthorizer}, {@link #serveFencedResource}, {@link #serveAdmittedFilter}, or a raw
 * {@link #handle}); payloads are decoded and encoded with {@link Ucv1}. A handler exception answers
 * {@code ERROR} with its message, a kind without a handler answers {@code UNIMPLEMENTED}. When Rust
 * stops waiting (timeout, abandoned caller, shutdown) the handler's thread is interrupted.
 *
 * <p>The Rust-side adapters built on a table: {@code site.aster.lease.NativeLeaseSerializer},
 * {@code site.aster.lease.NativeFencedResource}, {@link AdmittedFilterHandle} and {@link
 * Topology#enableShared}.
 */
public final class Upcalls implements AutoCloseable {

  /** {@code ASTER_UPCALL_DEFAULT_CAPACITY}. */
  public static final int DEFAULT_CAPACITY = 1024;

  /** {@code aster_upcall_status_t}. */
  public static final int STATUS_OK = 0;

  public static final int STATUS_ERROR = 1;
  public static final int STATUS_UNIMPLEMENTED = 2;

  /** {@code aster_upcall_cancel_reason_t}. */
  public static final int CANCEL_TIMEOUT = 1;

  public static final int CANCEL_ABANDONED = 2;
  public static final int CANCEL_SHUTDOWN = 3;

  /** Longest park between two empty polls. */
  public static final Duration MAX_IDLE_PARK = Duration.ofMillis(2);

  private static final int BATCH = 32;

  // ── aster_upcall_event_t (40 bytes, ffi/src/layout.rs) ───────────────────

  public static final MemoryLayout EVENT_LAYOUT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_LONG.withName("invocation_id"), // 0
          ValueLayout.JAVA_SHORT.withName("kind"), //          8
          ValueLayout.JAVA_BYTE.withName("cancel_reason"), // 10
          ValueLayout.JAVA_BYTE.withName("_p0"), //           11
          ValueLayout.JAVA_INT.withName("payload_len"), //    12
          ValueLayout.JAVA_LONG.withName("deadline_unix_ms"), // 16
          ValueLayout.ADDRESS.withName("payload_ptr"), //     24
          ValueLayout.JAVA_LONG.withName("buffer")); //       32 -> 40

  static {
    if (EVENT_LAYOUT.byteSize() != 40) {
      throw new AssertionError("aster_upcall_event_t layout drift");
    }
  }

  /** A raw handler: the kind's UCv1 request in, its UCv1 response out. */
  @FunctionalInterface
  public interface Handler {
    byte[] handle(byte[] request, long deadlineUnixMs) throws Exception;
  }

  /**
   * A handler that answers for itself, for kinds whose answer is not the end of the work (the
   * {@code expose_http} head is answered first, then the body streams). It must call {@link
   * Invocation#respond} once, or throw (which answers {@code ERROR} while Rust still waits).
   */
  @FunctionalInterface
  public interface InvocationHandler {
    void handle(Invocation invocation) throws Exception;
  }

  /**
   * One upcall in flight. When Rust stops waiting before the handler answers, the handler's thread
   * is interrupted and {@link #cancelReason()} says why.
   */
  public static final class Invocation {
    private final Upcalls table;
    private final Event event;
    private volatile int cancelReason;

    Invocation(Upcalls table, Event event) {
      this.table = table;
      this.event = event;
    }

    public long id() {
      return event.invocationId();
    }

    public byte[] payload() {
      return event.payload();
    }

    public long deadlineUnixMs() {
      return event.deadlineUnixMs();
    }

    /** {@link #CANCEL_TIMEOUT}, {@link #CANCEL_ABANDONED}, {@link #CANCEL_SHUTDOWN}, or 0. */
    public int cancelReason() {
      return cancelReason;
    }

    public boolean isCancelled() {
      return cancelReason != 0;
    }

    /** Answer; false when Rust no longer waits (see {@link Upcalls#respond}). */
    public boolean respond(int status, byte[] payload) {
      return table.respond(event.invocationId(), status, payload);
    }
  }

  private record Running(FutureTask<Void> task, Invocation invocation) {}

  /** One drained upcall event (payload copied, native buffer released). */
  public record Event(
      long invocationId, int kind, int cancelReason, long deadlineUnixMs, byte[] payload) {}

  private static final ValueLayout.OfInt I = UpcallNative.I;
  private static final ValueLayout.OfLong J = UpcallNative.J;
  private static final java.lang.foreign.AddressLayout A = UpcallNative.A;

  private static final MethodHandle NEW =
      UpcallNative.handle("aster_upcalls_new", FunctionDescriptor.of(I, J, I, A));
  private static final MethodHandle CLOSE =
      UpcallNative.handle("aster_upcalls_close", FunctionDescriptor.of(I, J, J));
  private static final MethodHandle FREE =
      UpcallNative.handle("aster_upcalls_free", FunctionDescriptor.of(I, J, J));
  private static final MethodHandle SET_POLICY =
      UpcallNative.handle("aster_upcall_set_policy", FunctionDescriptor.of(I, J, J, I, I, I));
  private static final MethodHandle SET_DEFAULT_POLICY =
      UpcallNative.handle("aster_upcall_set_default_policy", FunctionDescriptor.of(I, J, J, I, I));
  private static final MethodHandle POLL =
      UpcallNative.handle("aster_upcall_poll", FunctionDescriptor.of(I, J, J, A, I, I));
  private static final MethodHandle RESPOND =
      UpcallNative.handle("aster_upcall_respond", FunctionDescriptor.of(I, J, J, J, I, A, I));
  private static final MethodHandle BUFFER_RELEASE =
      UpcallNative.handle("aster_upcall_buffer_release", FunctionDescriptor.of(I, J, J, J));

  private final IrohRuntime runtime;
  private final long handle;
  private final Map<Integer, InvocationHandler> handlers = new ConcurrentHashMap<>();
  private final Map<Long, Running> inFlight = new ConcurrentHashMap<>();
  private final ExecutorService workers = Executors.newVirtualThreadPerTaskExecutor();
  private final Arena arena = Arena.ofShared();
  private final MemorySegment events;
  private final AtomicLong cancels = new AtomicLong();
  private volatile boolean closed;
  private final Thread pollThread;

  private Upcalls(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
    this.events = arena.allocate(EVENT_LAYOUT, BATCH);
    this.pollThread = Thread.ofVirtual().name("aster-upcalls-" + handle).start(this::pollLoop);
  }

  /** A table with the default capacity (1024 queued upcalls). */
  public static Upcalls create(IrohRuntime runtime) {
    return create(runtime, 0);
  }

  /**
   * A table queueing at most {@code capacity} upcalls (0 = default); beyond it upcalls are refused
   * at once (decisions take their failure mode, other kinds fail).
   */
  public static Upcalls create(IrohRuntime runtime, int capacity) {
    long h =
        UpcallNative.callForLong(
            "aster_upcalls_new",
            (a, out) -> (int) NEW.invoke(runtime.nativeHandle(), capacity, out));
    return new Upcalls(runtime, h);
  }

  public IrohRuntime runtime() {
    return runtime;
  }

  public long nativeHandle() {
    return handle;
  }

  /** Cancel events received so far (diagnostics / tests). */
  public long cancelCount() {
    return cancels.get();
  }

  // ── policy ───────────────────────────────────────────────────────────────

  /** Timeout and failure mode of one kind (default 5 s, fail closed). */
  public Upcalls setPolicy(UpcallKind kind, Duration timeout, FailureMode mode) {
    try {
      UpcallNative.check(
          (int)
              SET_POLICY.invoke(
                  runtime.nativeHandle(), handle, kind.code, millis(timeout), mode.code),
          "aster_upcall_set_policy");
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
    return this;
  }

  /** The policy of every kind without its own. */
  public Upcalls setDefaultPolicy(Duration timeout, FailureMode mode) {
    try {
      UpcallNative.check(
          (int)
              SET_DEFAULT_POLICY.invoke(runtime.nativeHandle(), handle, millis(timeout), mode.code),
          "aster_upcall_set_default_policy");
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
    return this;
  }

  private static int millis(Duration d) {
    return (int) Math.max(1, Math.min(Integer.MAX_VALUE, d.toMillis()));
  }

  // ── handlers ─────────────────────────────────────────────────────────────

  /** Register a raw handler for {@code kind} (replaces any earlier one). */
  public Upcalls handle(UpcallKind kind, Handler h) {
    return handleInvocation(
        kind,
        inv -> {
          byte[] out = h.handle(inv.payload(), inv.deadlineUnixMs());
          if (!Thread.currentThread().isInterrupted()) {
            inv.respond(STATUS_OK, out == null ? new byte[0] : out);
          }
        });
  }

  /** Register a handler that answers for itself (replaces any earlier one for the kind). */
  public Upcalls handleInvocation(UpcallKind kind, InvocationHandler h) {
    if (kind == UpcallKind.CANCEL) {
      throw new IllegalArgumentException("CANCEL is not answerable");
    }
    handlers.put(kind.code, h);
    return this;
  }

  /** Remove the handler of {@code kind}; later upcalls of it answer {@code UNIMPLEMENTED}. */
  public Upcalls removeHandler(UpcallKind kind) {
    handlers.remove(kind.code);
    return this;
  }

  /** Serve {@code LEASE_TRANSITION} and {@code LEASE_SNAPSHOT} with {@code s}. */
  public Upcalls serveLeaseSerializer(LeaseSerializer s) {
    handle(
        UpcallKind.LEASE_TRANSITION,
        (req, dl) -> {
          Ucv1.LeaseTransitionRequest r = Ucv1.decodeLeaseTransitionRequest(req);
          return Ucv1.encodeTransitionOutcome(s.transition(r.resource(), r.op()));
        });
    handle(
        UpcallKind.LEASE_SNAPSHOT,
        (req, dl) -> Ucv1.encodeLeaseSnapshot(s.snapshot(Ucv1.decodeLeaseSnapshotRequest(req))));
    return this;
  }

  /** Serve {@code LEASE_AUTHORIZE} with {@code a}. */
  public Upcalls serveLeaseAuthorizer(LeaseAuthorizer a) {
    return handle(
        UpcallKind.LEASE_AUTHORIZE,
        (req, dl) -> Ucv1.encodeDecision(a.authorize(Ucv1.decodeLeaseAuthorizeRequest(req))));
  }

  /** Serve {@code FENCED_MUTATE} with {@code r}. */
  public Upcalls serveFencedResource(FencedResource r) {
    return handle(
        UpcallKind.FENCED_MUTATE,
        (req, dl) -> {
          Ucv1.FencedMutateRequest m = Ucv1.decodeFencedMutateRequest(req);
          MutateResult res = r.mutate(m.fence(), m.writer(), m.op());
          if (res == null) {
            throw new IllegalStateException("FencedResource.mutate returned null");
          }
          return Ucv1.encodeFencedMutateResponse(res);
        });
  }

  /** Serve {@code TOPOLOGY_ADMITTED} with {@code f}. */
  public Upcalls serveAdmittedFilter(AdmittedFilter f) {
    return handle(
        UpcallKind.TOPOLOGY_ADMITTED,
        (req, dl) -> {
          List<String> ids = Ucv1.decodeTopologyAdmittedRequest(req);
          List<Boolean> answers = f.admitted(ids);
          if (answers == null || answers.size() != ids.size()) {
            throw new IllegalStateException(
                "AdmittedFilter must answer once per id (" + ids.size() + ")");
          }
          return Ucv1.encodeTopologyAdmittedResponse(answers);
        });
  }

  // ── poll loop ────────────────────────────────────────────────────────────

  private void pollLoop() {
    long parkNanos = 0;
    while (!closed) {
      int n;
      try {
        n = (int) POLL.invoke(runtime.nativeHandle(), handle, events, BATCH, 0);
      } catch (Throwable t) {
        if (closed) {
          return;
        }
        n = 0;
      }
      if (n == 0) {
        parkNanos = parkNanos == 0 ? 50_000 : Math.min(parkNanos * 2, MAX_IDLE_PARK.toNanos());
        LockSupport.parkNanos(parkNanos);
        continue;
      }
      parkNanos = 0;
      for (int i = 0; i < n; i++) {
        dispatch(read(events.asSlice(i * EVENT_LAYOUT.byteSize(), EVENT_LAYOUT.byteSize())));
      }
    }
  }

  private Event read(MemorySegment ev) {
    long id = ev.get(J, 0);
    int kind = ev.get(ValueLayout.JAVA_SHORT, 8) & 0xffff;
    int reason = ev.get(ValueLayout.JAVA_BYTE, 10) & 0xff;
    int len = ev.get(I, 12);
    long deadline = ev.get(J, 16);
    MemorySegment ptr = ev.get(A, 24);
    long buffer = ev.get(J, 32);
    byte[] payload = new byte[0];
    if (len > 0 && ptr.address() != 0) {
      payload = ptr.reinterpret(len).toArray(ValueLayout.JAVA_BYTE);
    }
    if (buffer != 0) {
      try {
        BUFFER_RELEASE.invoke(runtime.nativeHandle(), handle, buffer);
      } catch (Throwable ignored) {
        // copied already
      }
    }
    return new Event(id, kind, reason, deadline, payload);
  }

  private void dispatch(Event ev) {
    if (ev.kind() == UpcallKind.CANCEL.code) {
      cancels.incrementAndGet();
      Running r = inFlight.remove(ev.invocationId());
      if (r != null) {
        r.invocation().cancelReason = Math.max(1, ev.cancelReason());
        r.task().cancel(true);
      }
      return;
    }
    InvocationHandler h = handlers.get(ev.kind());
    if (h == null) {
      respond(ev.invocationId(), STATUS_UNIMPLEMENTED, new byte[0]);
      return;
    }
    long id = ev.invocationId();
    Invocation inv = new Invocation(this, ev);
    Running[] self = new Running[1];
    FutureTask<Void> task =
        new FutureTask<>(
            () -> {
              try {
                h.handle(inv);
              } catch (InterruptedException e) {
                // cancelled by Rust: nobody is waiting
              } catch (Throwable t) {
                if (!inv.isCancelled()) {
                  String msg = t.getMessage() == null ? t.toString() : t.getMessage();
                  respond(id, STATUS_ERROR, msg.getBytes(StandardCharsets.UTF_8));
                }
              } finally {
                inFlight.remove(id, self[0]);
              }
              return null;
            });
    self[0] = new Running(task, inv);
    inFlight.put(id, self[0]);
    try {
      workers.execute(task);
    } catch (RuntimeException rejected) {
      inFlight.remove(id, self[0]);
      respond(id, STATUS_ERROR, "upcalls closed".getBytes(StandardCharsets.UTF_8));
    }
  }

  /**
   * Answer {@code invocationId}. Returns false when Rust no longer waits for it (answered,
   * cancelled, timed out) -- the normal outcome of a late answer.
   */
  public boolean respond(long invocationId, int status, byte[] payload) {
    if (closed) {
      return false;
    }
    try (Arena a = Arena.ofConfined()) {
      MemorySegment seg = MemorySegment.NULL;
      if (payload.length > 0) {
        seg = a.allocate(payload.length);
        seg.copyFrom(MemorySegment.ofArray(payload));
      }
      int st =
          (int)
              RESPOND.invoke(
                  runtime.nativeHandle(), handle, invocationId, status, seg, payload.length);
      return st == 0;
    } catch (Throwable t) {
      return false;
    }
  }

  /** Close the table only: pending upcalls fail now (decisions fall back). Handlers stay. */
  public void closeTable() {
    try {
      CLOSE.invoke(runtime.nativeHandle(), handle);
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /**
   * Close the table (pending upcalls fail, decisions take their failure mode), stop the poll loop,
   * interrupt running handlers and free the native handle. Adapters built on this table can no
   * longer reach Java.
   */
  @Override
  public void close() {
    if (closed) {
      return;
    }
    try {
      CLOSE.invoke(runtime.nativeHandle(), handle);
    } catch (Throwable ignored) {
      // runtime already closed
    }
    closed = true;
    try {
      pollThread.join(Duration.ofSeconds(2));
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
    }
    inFlight.values().forEach(r -> r.task().cancel(true));
    workers.shutdownNow();
    try {
      workers.awaitTermination(2, TimeUnit.SECONDS);
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
    }
    try {
      FREE.invoke(runtime.nativeHandle(), handle);
    } catch (Throwable ignored) {
      // runtime already closed
    }
    arena.close();
  }
}
