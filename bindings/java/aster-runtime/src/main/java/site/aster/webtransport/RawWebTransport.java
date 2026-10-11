package site.aster.webtransport;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.*;
import java.util.concurrent.CompletableFuture;
import java.util.function.Consumer;
import java.util.function.Function;
import site.aster.event.IrohEvent;
import site.aster.ffi.*;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.Upcalls;

/**
 * Standalone, unframed WebTransport (ABI 4.17). Futures cancel native work immediately. A cancelled
 * write may have delivered a prefix; callers must not replay it automatically.
 */
public final class RawWebTransport {
  private RawWebTransport() {}

  private static final ValueLayout.OfInt I = ValueLayout.JAVA_INT;
  private static final ValueLayout.OfLong J = ValueLayout.JAVA_LONG;
  private static final AddressLayout A = ValueLayout.ADDRESS;
  private static final MemoryLayout B = MemoryLayout.structLayout(A, J);
  public static final MemoryLayout CONFIG_LAYOUT =
      MemoryLayout.structLayout(I, I, B, B, B, B, I, I, I, I, I, I, I, I);
  private static final ObjectMapper JSON = new ObjectMapper();

  /**
   * PEM bytes are copied; zero limits select bounded native defaults. Exact path is not normalized.
   */
  public record Config(
      String bindAddress,
      byte[] certificatePem,
      byte[] privateKeyPem,
      String path,
      int maxSessions,
      int maxStreams,
      int maxDatagrams,
      int maxDatagramBytes,
      int maxReadBytes,
      int maxPendingAuth,
      int maxAuthPayloadBytes) {
    public Config {
      Objects.requireNonNull(bindAddress);
      Objects.requireNonNull(path);
      certificatePem = certificatePem.clone();
      privateKeyPem = privateKeyPem.clone();
      for (int v :
          new int[] {
            maxSessions,
            maxStreams,
            maxDatagrams,
            maxDatagramBytes,
            maxReadBytes,
            maxPendingAuth,
            maxAuthPayloadBytes
          }) if (v < 0) throw new IllegalArgumentException("limits must be nonnegative");
    }

    public Config(String bind, byte[] cert, byte[] key, String path) {
      this(bind, cert, key, path, 0, 0, 0, 0, 0, 0, 0);
    }

    @Override
    public byte[] certificatePem() {
      return certificatePem.clone();
    }

    @Override
    public byte[] privateKeyPem() {
      return privateKeyPem.clone();
    }
  }

  /** Empty bytes with eof=true are stream EOF; empty datagrams remain valid payloads. */
  public record Read(byte[] bytes, boolean eof) {}

  private static MethodHandle h(String name, MemoryLayout... args) {
    return IrohLibrary.getInstance().getHandle(name, FunctionDescriptor.of(I, args));
  }

  private static int call(String name, MemoryLayout[] layouts, Object... args) {
    try {
      return (int) h(name, layouts).invokeWithArguments(args);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  private static RuntimeException rethrow(Throwable t) {
    if (t instanceof RuntimeException e) return e;
    if (t instanceof Error e) throw e;
    return new RuntimeException(t);
  }

  private static void check(int s) {
    if (s != 0) throw IrohException.forStatus(IrohStatus.fromCode(s), DataPlane.lastError());
  }

  private static MemorySegment bytes(Arena arena, byte[] bytes) {
    if (bytes.length > 16 * 1024 * 1024) throw new IllegalArgumentException("input exceeds 16 MiB");
    MemorySegment b = arena.allocate(B);
    b.set(
        A,
        0,
        bytes.length == 0 ? MemorySegment.NULL : arena.allocateFrom(ValueLayout.JAVA_BYTE, bytes));
    b.set(J, 8, bytes.length);
    return b;
  }

  private static void put(Arena a, MemorySegment cfg, long offset, byte[] bytes) {
    MemorySegment.copy(bytes(a, bytes), 0, cfg, offset, 16);
  }

  private static byte[] payload(IrohEvent e) {
    return e.dataLen() == 0 ? new byte[0] : e.data().toArray(ValueLayout.JAVA_BYTE);
  }

  private static JsonNode parse(byte[] bytes) {
    try {
      return JSON.readTree(bytes);
    } catch (java.io.IOException e) {
      throw new IllegalStateException("invalid native JSON", e);
    }
  }

  private static JsonNode info(String name, IrohRuntime rt, long handle) {
    try (Arena a = Arena.ofConfined()) {
      MemorySegment out = a.allocate(B);
      check(call(name, new MemoryLayout[] {J, J, A}, rt.nativeHandle(), handle, out));
      MemorySegment ptr = out.get(A, 0);
      long len = out.get(J, 8);
      try {
        return parse(ptr.reinterpret(len).toArray(ValueLayout.JAVA_BYTE));
      } finally {
        check(call("aster_string_release", new MemoryLayout[] {A, J}, ptr, len));
      }
    }
  }

  private static void free(String name, IrohRuntime rt, long handle) {
    int s = call(name, new MemoryLayout[] {J, J}, rt.nativeHandle(), handle);
    if (s != IrohStatus.NOT_FOUND.code && s != IrohStatus.ALREADY_CLOSED.code) check(s);
  }

  private abstract static class Owner implements AutoCloseable {
    final IrohRuntime runtime;
    private long handle;

    Owner(IrohRuntime runtime, long handle) {
      this.runtime = runtime;
      this.handle = handle;
    }

    synchronized long handle() {
      if (handle == 0) throw new IllegalStateException("owner closed");
      return handle;
    }

    synchronized long take() {
      long h = handle;
      handle = 0;
      return h;
    }

    public synchronized boolean isClosed() {
      return handle == 0;
    }

    <T> CompletableFuture<T> submit(
        String name,
        int kind,
        MemoryLayout[] extras,
        Object[] args,
        Function<IrohEvent, T> map,
        Consumer<T> discard) {
      long owner = handle();
      long op;
      try (Arena arena = Arena.ofConfined()) {
        List<MemoryLayout> layouts = new ArrayList<>(List.of(J, J));
        layouts.addAll(Arrays.asList(extras));
        layouts.add(J);
        layouts.add(A);
        List<Object> values = new ArrayList<>(List.of(runtime.nativeHandle(), owner));
        values.addAll(Arrays.asList(args));
        values.add(0L);
        MemorySegment out = arena.allocate(J);
        values.add(out);
        check(call(name, layouts.toArray(MemoryLayout[]::new), values.toArray()));
        op = out.get(J, 0);
      }
      CompletableFuture<T> result =
          new CompletableFuture<>() {
            @Override
            public boolean cancel(boolean interrupt) {
              boolean won = super.cancel(interrupt);
              if (won) {
                try {
                  runtime.cancelOperation(op);
                } catch (IrohException e) {
                  if (e.status() != IrohStatus.NOT_FOUND && e.status() != IrohStatus.ALREADY_CLOSED)
                    throw e;
                }
              }
              return won;
            }
          };
      // Keep collecting even after managed cancellation: native completion may already have won.
      runtime
          .registry()
          .register(op)
          .whenComplete(
              (event, error) -> {
                if (error != null) {
                  result.completeExceptionally(error);
                  return;
                }
                try {
                  if (event.kind().code != kind)
                    throw new IllegalStateException(
                        "unexpected raw WebTransport event: " + event.kind());
                  T value = map.apply(event);
                  if (!result.complete(value)) discard.accept(value);
                } catch (Throwable t) {
                  result.completeExceptionally(t);
                } finally {
                  if (event.hasBuffer()) runtime.releaseBuffer(event.buffer());
                }
              });
      return result;
    }

    <T> CompletableFuture<T> submit(
        String name, int kind, Function<IrohEvent, T> map, Consumer<T> discard) {
      return submit(name, kind, new MemoryLayout[0], new Object[0], map, discard);
    }

    CompletableFuture<Void> unit(String name, MemoryLayout[] extra, Object... args) {
      return submit(name, 92, extra, args, e -> null, v -> {});
    }
  }

  public static Listener startPublic(IrohRuntime runtime, Config config) {
    return startInternal(runtime, null, config);
  }

  public static Listener start(IrohRuntime runtime, Upcalls upcalls, Config config) {
    return startInternal(runtime, Objects.requireNonNull(upcalls), config);
  }

  private static Listener startInternal(IrohRuntime runtime, Upcalls upcalls, Config config) {
    Objects.requireNonNull(runtime);
    Objects.requireNonNull(config);
    if (upcalls != null && (upcalls.runtime() != runtime || !upcalls.hasRpcAuthenticator()))
      throw new IllegalArgumentException(
          "authenticated listener requires runtime-owned RPC authenticator");
    try (Arena a = Arena.ofConfined()) {
      MemorySegment c = a.allocate(CONFIG_LAYOUT);
      c.set(I, 0, 104);
      put(a, c, 8, config.bindAddress().getBytes(StandardCharsets.UTF_8));
      put(a, c, 24, config.certificatePem());
      put(a, c, 40, config.privateKeyPem());
      put(a, c, 56, config.path().getBytes(StandardCharsets.UTF_8));
      int[] limits = {
        config.maxSessions(),
        config.maxStreams(),
        config.maxDatagrams(),
        config.maxDatagramBytes(),
        config.maxReadBytes(),
        config.maxPendingAuth(),
        config.maxAuthPayloadBytes()
      };
      for (int n = 0; n < limits.length; n++) c.set(I, 72 + n * 4, limits[n]);
      MemorySegment out = a.allocate(J);
      check(
          upcalls == null
              ? call(
                  "aster_raw_wt_listener_start_public",
                  new MemoryLayout[] {J, A, A},
                  runtime.nativeHandle(),
                  c,
                  out)
              : call(
                  "aster_raw_wt_listener_start",
                  new MemoryLayout[] {J, J, A, A},
                  runtime.nativeHandle(),
                  upcalls.nativeHandle(),
                  c,
                  out));
      return new Listener(runtime, out.get(J, 0), upcalls);
    }
  }

  /** Owned listener; stop joins native work and releases bound ports. */
  public static final class Listener extends Owner {
    private final Upcalls upcalls;

    private Listener(IrohRuntime rt, long h, Upcalls upcalls) {
      super(rt, h);
      this.upcalls = upcalls;
    }

    public JsonNode addresses() {
      return info("aster_raw_wt_listener_addr", runtime, handle());
    }

    public CompletableFuture<Session> accept() {
      return submit(
          "aster_raw_wt_listener_accept",
          121,
          e -> new Session(runtime, e.handle()),
          Session::close);
    }

    public synchronized void stop(Duration grace) {
      long ms = grace.toMillis();
      if (grace.isNegative() || ms > 0xffff_ffffL)
        throw new IllegalArgumentException("invalid grace");
      if (isClosed()) return;
      int s =
          call(
              "aster_raw_wt_listener_stop",
              new MemoryLayout[] {J, J, I},
              runtime.nativeHandle(),
              handle(),
              (int) ms);
      if (s != IrohStatus.NOT_FOUND.code && s != IrohStatus.ALREADY_CLOSED.code) check(s);
      take();
    }

    @Override
    public void close() {
      stop(Duration.ZERO);
    }
  }

  /**
   * Session info/stats are extensible version1 JSON, including authenticated principal/attributes.
   */
  public static final class Session extends Owner {
    private Session(IrohRuntime rt, long h) {
      super(rt, h);
    }

    public JsonNode info() {
      return RawWebTransport.info("aster_raw_wt_session_info", runtime, handle());
    }

    public JsonNode stats() {
      return RawWebTransport.info("aster_raw_wt_session_stats", runtime, handle());
    }

    private CompletableFuture<Stream> stream(String name) {
      return submit(
          "aster_raw_wt_session_" + name,
          122,
          e -> new Stream(runtime, e.handle(), e.flags()),
          Stream::close);
    }

    public CompletableFuture<Stream> acceptBidirectional() {
      return stream("accept_bi");
    }

    public CompletableFuture<Stream> acceptUnidirectional() {
      return stream("accept_uni");
    }

    public CompletableFuture<Stream> openBidirectional() {
      return stream("open_bi");
    }

    public CompletableFuture<Stream> openUnidirectional() {
      return stream("open_uni");
    }

    public CompletableFuture<byte[]> receiveDatagram() {
      return submit(
          "aster_raw_wt_session_receive_datagram", 124, RawWebTransport::payload, v -> {});
    }

    public void sendDatagram(byte[] data) {
      try (Arena a = Arena.ofConfined()) {
        check(
            call(
                "aster_raw_wt_session_send_datagram",
                new MemoryLayout[] {J, J, B},
                runtime.nativeHandle(),
                handle(),
                bytes(a, data)));
      }
    }

    public OptionalLong maxDatagramSize() {
      try (Arena a = Arena.ofConfined()) {
        MemorySegment size = a.allocate(J), some = a.allocate(I);
        check(
            call(
                "aster_raw_wt_session_max_datagram_size",
                new MemoryLayout[] {J, J, A, A},
                runtime.nativeHandle(),
                handle(),
                size,
                some));
        return some.get(I, 0) == 0 ? OptionalLong.empty() : OptionalLong.of(size.get(J, 0));
      }
    }

    public CompletableFuture<JsonNode> closed() {
      return submit("aster_raw_wt_session_closed", 125, e -> parse(payload(e)), v -> {});
    }

    public void close(int code, byte[] reason) {
      try (Arena a = Arena.ofConfined()) {
        check(
            call(
                "aster_raw_wt_session_close",
                new MemoryLayout[] {J, J, I, B},
                runtime.nativeHandle(),
                handle(),
                code,
                bytes(a, reason)));
      }
    }

    @Override
    public void close() {
      long h = take();
      if (h != 0) free("aster_raw_wt_session_free", runtime, h);
    }
  }

  /** Single directional owner. FIN keeps its send half available for ACK/reset. */
  public static final class Stream extends Owner {
    private final int directions;

    private Stream(IrohRuntime rt, long h, int directions) {
      super(rt, h);
      this.directions = directions;
    }

    public boolean hasSend() {
      return (directions & 1) != 0;
    }

    public boolean hasReceive() {
      return (directions & 2) != 0;
    }

    public JsonNode info() {
      return RawWebTransport.info("aster_raw_wt_stream_info", runtime, handle());
    }

    public CompletableFuture<Read> read(int maximum) {
      if (maximum <= 0) throw new IllegalArgumentException("positive read maximum required");
      return submit(
          "aster_raw_wt_stream_read",
          123,
          new MemoryLayout[] {I},
          new Object[] {maximum},
          e -> new Read(payload(e), (e.flags() & 1) != 0),
          v -> {});
    }

    public CompletableFuture<Void> write(byte[] data) {
      try (Arena a = Arena.ofConfined()) {
        return unit("aster_raw_wt_stream_write", new MemoryLayout[] {B}, bytes(a, data));
      }
    }

    public CompletableFuture<Void> finish() {
      return unit("aster_raw_wt_stream_finish", new MemoryLayout[0]);
    }

    public CompletableFuture<Void> reset(int code) {
      return unit("aster_raw_wt_stream_reset", new MemoryLayout[] {I}, code);
    }

    public CompletableFuture<Void> stop(int code) {
      return unit("aster_raw_wt_stream_stop", new MemoryLayout[] {I}, code);
    }

    public CompletableFuture<Void> setPriority(int priority) {
      return unit("aster_raw_wt_stream_set_priority", new MemoryLayout[] {I}, priority);
    }

    public CompletableFuture<OptionalInt> priority() {
      return submit(
          "aster_raw_wt_stream_priority",
          91,
          e ->
              (e.flags() & 1) == 0
                  ? OptionalInt.empty()
                  : OptionalInt.of(
                      ByteBuffer.wrap(payload(e)).order(ByteOrder.LITTLE_ENDIAN).getInt()),
          v -> {});
    }

    public CompletableFuture<Boolean> transportAcked() {
      return submit("aster_raw_wt_stream_transport_acked", 126, e -> (e.flags() & 1) != 0, v -> {});
    }

    @Override
    public void close() {
      long h = take();
      if (h != 0) free("aster_raw_wt_stream_free", runtime, h);
    }
  }
}
