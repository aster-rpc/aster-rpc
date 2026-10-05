// The Java side of the InteropEcho contract (tests/interop/): a leg of the
// tests/interop/run.py matrix (contract / server / client) and the Java side
// of the dispatch benchmark (benchmarks/dispatch/run.py).
//
// Contract (must match tests/interop/rust/contract.rs):
//
//   service InteropEcho v1
//     echo(interop/EchoRequest) -> interop/EchoReply                (unary)
//     generate(interop/EchoRequest) -> stream interop/EchoReply     (server stream)
//     ingest(stream interop/EchoRequest) -> interop/EchoReply       (client stream)
//     chat(stream interop/EchoRequest) -> stream interop/EchoReply  (bidi)
//     hold(interop/EchoRequest) -> interop/EchoReply                (unary, blocks)
//     cancelled(interop/EchoRequest) -> interop/EchoReply           (unary)
//     traceEcho(interop/EchoRequest) -> interop/EchoReply           (unary)
//   service InteropSession v1 (session-scoped): add(interop/EchoRequest) -> interop/EchoReply
//
// Expose mini-matrix: `expose-server` serves HTTP service interop-http on ALPN
// aster-expose/interop/1 (GET /hello, POST /echo, header x-server-lang); see
// tests/interop/rust/interop_echo.rs.
//
// Single-file source program (JEP 330), no build step of its own:
//
//   java --enable-native-access=ALL-UNNAMED -cp <aster-runtime classpath> \
//       tests/interop/java/InteropEcho.java contract | server | client <aster1-ticket>
//   java ... InteropEcho.java expose-server
//   java ... InteropEcho.java bench <aster1-ticket> [--duration S] [--warmup S]
//       [--concurrency 1,10,50,100] [--stream-frames 100,10000] [--client-stream 10000]
//
// IROH_LIB_PATH points at libaster_transport_ffi (ABI 4.x). Every mode
// prints CONTRACT_ID <hex> first; `server` prints ADDRESS <aster1...> and
// serves until stdin closes (loopback, relays off, via aster_node_start);
// `client` prints CASE <name> OK|FAIL lines (same cases as the other
// languages); `bench` prints RESULT {json} lines in the schema of
// benchmarks/dispatch/driver.rs.

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import org.apache.fory.BaseFory;
import org.apache.fory.Fory;
import site.aster.annotations.Scope;
import site.aster.annotations.WireType;
import site.aster.client.AsterClient;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.codec.ForyTags;
import site.aster.config.AsterConfig;
import site.aster.contract.ContractManifestBuilder;
import site.aster.client.BidiCall;
import site.aster.client.ClientSession;
import site.aster.client.ServerStreamCall;
import site.aster.expose.Expose;
import site.aster.expose.HttpResponse;
import site.aster.ffi.Rpc;
import site.aster.handle.IrohConnection;
import site.aster.interceptors.CallContext;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.IrohNode;
import site.aster.server.spi.BidiStreamDispatcher;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;
import site.aster.server.spi.ClientStreamDispatcher;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStream;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ResponseStream;
import site.aster.server.spi.ServerStreamDispatcher;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

public final class InteropEcho {

  static final String REQ_TAG = "interop/EchoRequest";
  static final String REPLY_TAG = "interop/EchoReply";

  /** interop/EchoRequest { message: string, count: varint32, data: binary }. */
  @WireType(REQ_TAG)
  public static final class EchoRequest {
    public String message = "";
    public int count;
    public byte[] data = new byte[0];

    public EchoRequest() {}

    EchoRequest(String message, int count, byte[] data) {
      this.message = message;
      this.count = count;
      this.data = data;
    }
  }

  /**
   * interop/EchoReply { message, count, data, server_lang }. The field is spelled server_lang (not
   * serverLang) because the contract hashes field names verbatim.
   */
  @WireType(REPLY_TAG)
  public static final class EchoReply {
    public String message = "";
    public int count;
    public byte[] data = new byte[0];
    public String server_lang = "";

    public EchoReply() {}

    EchoReply(String message, int count, byte[] data, String serverLang) {
      this.message = message;
      this.count = count;
      this.data = data;
      this.server_lang = serverLang;
    }
  }

  static void registerTypes(BaseFory fory) {
    try {
      ForyTags.register(fory, EchoRequest.class, REQ_TAG);
    } catch (Throwable ignored) {
      // already registered
    }
    try {
      ForyTags.register(fory, EchoReply.class, REPLY_TAG);
    } catch (Throwable ignored) {
      // already registered
    }
  }

  /** The service implementation (same semantics as every other language). */
  public static final class Impl {}

  static final class Dispatcher implements ServiceDispatcher {
    private final ServiceDescriptor descriptor =
        new ServiceDescriptor("InteropEcho", 1, Scope.SHARED, Impl.class);

    @Override
    public ServiceDescriptor descriptor() {
      return descriptor;
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      return Map.of(
          "echo", new Echo(),
          "generate", new Generate(),
          "ingest", new Ingest(),
          "chat", new Chat(),
          "hold", new Hold(),
          "cancelled", new Cancelled(),
          "traceEcho", new TraceEcho());
    }

    @Override
    public Map<String, Class<?>> requestClasses() {
      return Map.of(
          "echo", EchoRequest.class, "generate", EchoRequest.class, "ingest", EchoRequest.class,
          "chat", EchoRequest.class, "hold", EchoRequest.class, "cancelled", EchoRequest.class,
          "traceEcho", EchoRequest.class);
    }

    @Override
    public Map<String, Class<?>> responseClasses() {
      return Map.of(
          "echo", EchoReply.class, "generate", EchoReply.class, "ingest", EchoReply.class,
          "chat", EchoReply.class, "hold", EchoReply.class, "cancelled", EchoReply.class,
          "traceEcho", EchoReply.class);
    }

    @Override
    public void registerTypes(Fory fory) {
      InteropEcho.registerTypes(fory);
    }
  }

  static MethodDescriptor desc(String name, StreamingKind kind) {
    return new MethodDescriptor(
        name, kind, RequestStyle.EXPLICIT, REQ_TAG, List.of(), REPLY_TAG, false, false);
  }

  static final class Echo implements UnaryDispatcher {
    @Override
    public MethodDescriptor descriptor() {
      return desc("echo", StreamingKind.UNARY);
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      EchoRequest req = (EchoRequest) codec.decode(requestBytes, EchoRequest.class);
      return codec.encode(new EchoReply(req.message, req.count + 1, req.data, "java"));
    }
  }

  static final class Generate implements ServerStreamDispatcher {
    @Override
    public MethodDescriptor descriptor() {
      return desc("generate", StreamingKind.SERVER_STREAM);
    }

    @Override
    public void invoke(
        Object impl, byte[] requestBytes, Codec codec, CallContext ctx, ResponseStream out)
        throws Exception {
      EchoRequest req = (EchoRequest) codec.decode(requestBytes, EchoRequest.class);
      for (int i = 0; i < Math.max(req.count, 0); i++) {
        out.send(codec.encode(new EchoReply(req.message, i, req.data, "java")));
      }
    }
  }

  static final class Ingest implements ClientStreamDispatcher {
    @Override
    public MethodDescriptor descriptor() {
      return desc("ingest", StreamingKind.CLIENT_STREAM);
    }

    @Override
    public byte[] invoke(Object impl, RequestStream in, Codec codec, CallContext ctx)
        throws Exception {
      int n = 0;
      String last = "";
      while (true) {
        byte[] payload = in.receive();
        if (payload == null) {
          break;
        }
        EchoRequest r = (EchoRequest) codec.decode(payload, EchoRequest.class);
        n++;
        last = r.message;
      }
      return codec.encode(new EchoReply(last, n, new byte[0], "java"));
    }
  }

  /** {@code chat}: one reply per request, sent right away; count = the request's position. */
  static final class Chat implements BidiStreamDispatcher {
    @Override
    public MethodDescriptor descriptor() {
      return desc("chat", StreamingKind.BIDI_STREAM);
    }

    @Override
    public void invoke(
        Object impl, RequestStream in, Codec codec, CallContext ctx, ResponseStream out)
        throws Exception {
      int i = 0;
      while (true) {
        byte[] payload = in.receive();
        if (payload == null) {
          break;
        }
        EchoRequest r = (EchoRequest) codec.decode(payload, EchoRequest.class);
        out.send(codec.encode(new EchoReply(r.message, i++, r.data, "java")));
      }
    }
  }

  /** {@code hold} calls that observed a cancellation (process-wide). */
  static final AtomicInteger HOLD_CANCELLED = new AtomicInteger();

  /**
   * {@code hold}: blocks until cancelled (a client cancel, seen as {@code ctx.isCancelled()}, or
   * the call's deadline, by watching the clock) or 60 s.
   */
  static final class Hold implements UnaryDispatcher {
    @Override
    public MethodDescriptor descriptor() {
      return desc("hold", StreamingKind.UNARY);
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      Double deadline = ctx.deadline();
      long end = System.nanoTime() + 60_000_000_000L;
      while (System.nanoTime() < end) {
        if (ctx.isCancelled()) {
          HOLD_CANCELLED.incrementAndGet();
          throw new RpcError(StatusCode.CANCELLED, "hold: cancelled");
        }
        if (deadline != null && System.currentTimeMillis() / 1000.0 >= deadline) {
          HOLD_CANCELLED.incrementAndGet();
          throw new RpcError(StatusCode.DEADLINE_EXCEEDED, "hold: deadline passed");
        }
        try {
          Thread.sleep(20);
        } catch (InterruptedException e) {
          HOLD_CANCELLED.incrementAndGet();
          Thread.currentThread().interrupt();
          throw new RpcError(StatusCode.CANCELLED, "hold: interrupted");
        }
      }
      return codec.encode(new EchoReply("timeout", -1, new byte[0], "java"));
    }
  }

  static final class Cancelled implements UnaryDispatcher {
    @Override
    public MethodDescriptor descriptor() {
      return desc("cancelled", StreamingKind.UNARY);
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      return codec.encode(new EchoReply("cancelled", HOLD_CANCELLED.get(), new byte[0], "java"));
    }
  }

  /**
   * {@code traceEcho}: replies the trace id (32 hex) of the handler's dispatch context, read from
   * the {@code site.aster.otel.Tracing} carrier the dispatcher fills.
   */
  static final class TraceEcho implements UnaryDispatcher {
    @Override
    public MethodDescriptor descriptor() {
      return desc("traceEcho", StreamingKind.UNARY);
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      String traceId = site.aster.otel.Tracing.currentTraceId();
      return codec.encode(new EchoReply(traceId == null ? "" : traceId, 0, new byte[0], "java"));
    }
  }

  // ---- session-scoped InteropSession --------------------------------------

  /** One instance per (connection, session id). */
  public static final class SessionImpl {
    int total;
    int calls;
  }

  static final class SessionDispatcher implements ServiceDispatcher {
    private final ServiceDescriptor descriptor =
        new ServiceDescriptor("InteropSession", 1, Scope.SESSION, SessionImpl.class);

    @Override
    public ServiceDescriptor descriptor() {
      return descriptor;
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      return Map.of("add", new Add());
    }

    @Override
    public Map<String, Class<?>> requestClasses() {
      return Map.of("add", EchoRequest.class);
    }

    @Override
    public Map<String, Class<?>> responseClasses() {
      return Map.of("add", EchoReply.class);
    }

    @Override
    public void registerTypes(Fory fory) {
      InteropEcho.registerTypes(fory);
    }
  }

  static final class Add implements UnaryDispatcher {
    @Override
    public MethodDescriptor descriptor() {
      return desc("add", StreamingKind.UNARY);
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      SessionImpl s = (SessionImpl) impl;
      EchoRequest r = (EchoRequest) codec.decode(requestBytes, EchoRequest.class);
      int total;
      int calls;
      synchronized (s) {
        s.total += r.count;
        s.calls++;
        total = s.total;
        calls = s.calls;
      }
      return codec.encode(new EchoReply("calls=" + calls, total, new byte[0], "java"));
    }
  }

  static AsterConfig config() {
    return AsterConfig.builder()
        .relayMode("disabled")
        .bindAddr("127.0.0.1:0")
        .allowAllConsumers(true)
        .build();
  }

  static void serve() throws Exception {
    ForyCodec codec = new ForyCodec();
    registerTypes(codec.fory());
    AsterServer server =
        AsterServer.builder()
            .config(config())
            .codec(codec)
            .service(new Impl(), new Dispatcher())
            .sessionService(SessionImpl.class, peer -> new SessionImpl(), new SessionDispatcher())
            .build()
            .get(30, TimeUnit.SECONDS);
    NodeAddr addr = server.node().nodeAddr();
    for (int i = 0; i < 20 && addr.directAddresses().isEmpty(); i++) {
      Thread.sleep(50);
      addr = server.node().nodeAddr();
    }
    System.err.println("node addr: " + addr);
    System.out.println("ADDRESS " + addr.toTicket());
    System.out.flush();
    BufferedReader stdin = new BufferedReader(new InputStreamReader(System.in));
    while (stdin.readLine() != null) {
      // serve until stdin closes
    }
    server.close();
    System.exit(0);
  }

  // ---- bench client -------------------------------------------------------

  static EchoRequest benchRequest() {
    byte[] d = new byte[64];
    Arrays.fill(d, (byte) 0xab);
    return new EchoRequest("bench", 1, d);
  }

  static EchoRequest streamRequest(int frames) {
    byte[] d = new byte[64];
    Arrays.fill(d, (byte) 0xcd);
    return new EchoRequest("s", frames, d);
  }

  static double pct(long[] sorted, double p) {
    if (sorted.length == 0) {
      return 0;
    }
    return sorted[Math.min((int) (sorted.length * p), sorted.length - 1)] / 1e6;
  }

  static double median(List<Double> v) {
    List<Double> s = new ArrayList<>(v);
    s.sort(Double::compare);
    return s.get(s.size() / 2);
  }

  record UnaryRun(long calls, long errors, double secs, long[] lats) {}

  /** Closed loop: conc virtual threads issue blocking calls back to back for duration. */
  static UnaryRun runUnary(AsterClient client, NodeAddr addr, int conc, double durationS)
      throws Exception {
    AtomicBoolean stop = new AtomicBoolean(false);
    AtomicLong errors = new AtomicLong();
    List<long[]> buckets = new ArrayList<>();
    List<int[]> counts = new ArrayList<>();
    List<Thread> threads = new ArrayList<>();
    EchoRequest req = benchRequest();
    for (int i = 0; i < conc; i++) {
      long[][] holder = {new long[1 << 16]};
      int[] n = {0};
      counts.add(n);
      Thread t =
          Thread.ofVirtual()
              .start(
                  () -> {
                    while (!stop.get()) {
                      long t0 = System.nanoTime();
                      try {
                        EchoReply r =
                            client
                                .<EchoRequest, EchoReply>call(
                                    addr, "InteropEcho", "echo", req, EchoReply.class)
                                .get(30, TimeUnit.SECONDS);
                        if (r.count != 2) {
                          throw new IllegalStateException("bad reply count " + r.count);
                        }
                        long dt = System.nanoTime() - t0;
                        if (n[0] == holder[0].length) {
                          holder[0] = Arrays.copyOf(holder[0], holder[0].length * 2);
                        }
                        holder[0][n[0]++] = dt;
                      } catch (Exception e) {
                        if (errors.getAndIncrement() == 0) {
                          System.err.println("worker error: " + e);
                        }
                      }
                    }
                    synchronized (buckets) {
                      buckets.add(Arrays.copyOf(holder[0], n[0]));
                    }
                  });
      threads.add(t);
    }
    long start = System.nanoTime();
    Thread.sleep((long) (durationS * 1000));
    stop.set(true);
    for (Thread t : threads) {
      t.join();
    }
    double secs = (System.nanoTime() - start) / 1e9;
    long[] all = buckets.stream().flatMapToLong(Arrays::stream).sorted().toArray();
    return new UnaryRun(all.length, errors.get(), secs, all);
  }

  static List<Integer> ints(String s) {
    List<Integer> out = new ArrayList<>();
    for (String p : s.split(",")) {
      if (!p.isEmpty()) {
        out.add(Integer.parseInt(p));
      }
    }
    return out;
  }

  static void bench(String[] args) throws Exception {
    String ticket = args[1];
    double duration = 5;
    double warmup = 1;
    String conc = "1,10,50,100";
    String frames = "100,10000";
    String clientStream = "10000";
    boolean skipStreams = false;
    for (int i = 2; i < args.length; i++) {
      switch (args[i]) {
        case "--duration" -> duration = Double.parseDouble(args[++i]);
        case "--warmup" -> warmup = Double.parseDouble(args[++i]);
        case "--concurrency" -> conc = args[++i];
        case "--stream-frames" -> frames = args[++i];
        case "--client-stream" -> clientStream = args[++i];
        case "--skip-streams" -> skipStreams = true;
        default -> throw new IllegalArgumentException("unknown arg " + args[i]);
      }
    }
    ForyCodec codec = new ForyCodec();
    registerTypes(codec.fory());
    NodeAddr addr = NodeAddr.fromTicket(ticket);
    try (AsterClient client =
        AsterClient.builder()
            .config(config())
            .codec(codec)
            .consumerAdmission(false)
            .build()
            .get(30, TimeUnit.SECONDS)) {
      EchoReply first =
          client
              .<EchoRequest, EchoReply>call(
                  addr, "InteropEcho", "echo", benchRequest(), EchoReply.class)
              .get(30, TimeUnit.SECONDS);
      if (first.count != 2) {
        throw new IllegalStateException("bad first reply count " + first.count);
      }
      if (warmup > 0) {
        runUnary(client, addr, 10, warmup);
      }
      for (int c : ints(conc)) {
        if (warmup > 0) {
          runUnary(client, addr, c, Math.min(warmup, 1));
        }
        UnaryRun r = runUnary(client, addr, c, duration);
        System.out.println(
            String.format(
                Locale.ROOT,
                "RESULT {\"kind\":\"unary\",\"mode\":\"java\",\"conc\":%d,\"calls\":%d,"
                    + "\"errors\":%d,\"secs\":%.3f,\"rps\":%.1f,\"p50_ms\":%.3f,"
                    + "\"p90_ms\":%.3f,\"p99_ms\":%.3f}",
                c,
                r.calls(),
                r.errors(),
                r.secs(),
                r.calls() / r.secs(),
                pct(r.lats(), 0.5),
                pct(r.lats(), 0.9),
                pct(r.lats(), 0.99)));
      }
      if (!skipStreams) {
        for (int f : ints(frames)) {
          List<Double> rates = new ArrayList<>();
          long budget = System.nanoTime();
          while (rates.size() < 3
              || (System.nanoTime() - budget < 2_000_000_000L && rates.size() < 200)) {
            long t0 = System.nanoTime();
            List<EchoReply> got =
                client
                    .<EchoRequest, EchoReply>callServerStream(
                        addr, "InteropEcho", "generate", streamRequest(f), EchoReply.class)
                    .get(120, TimeUnit.SECONDS);
            if (got.size() != f) {
              throw new IllegalStateException("server-stream got " + got.size() + " != " + f);
            }
            rates.add(f / ((System.nanoTime() - t0) / 1e9));
          }
          System.out.println(
              String.format(
                  Locale.ROOT,
                  "RESULT {\"kind\":\"server_stream\",\"frames\":%d,\"iterations\":%d,"
                      + "\"frames_per_s\":%.1f}",
                  f,
                  rates.size(),
                  median(rates)));
        }
        for (int n : ints(clientStream)) {
          List<Double> rates = new ArrayList<>();
          for (int k = 0; k < 3; k++) {
            List<EchoRequest> reqs = new ArrayList<>(n);
            for (int i = 0; i < n; i++) {
              reqs.add(streamRequest(1));
            }
            long t0 = System.nanoTime();
            EchoReply r =
                client
                    .<EchoRequest, EchoReply>callClientStream(
                        addr, "InteropEcho", "ingest", reqs, EchoReply.class)
                    .get(120, TimeUnit.SECONDS);
            if (r.count != n) {
              throw new IllegalStateException("client-stream count " + r.count + " != " + n);
            }
            rates.add(n / ((System.nanoTime() - t0) / 1e9));
          }
          System.out.println(
              String.format(
                  Locale.ROOT,
                  "RESULT {\"kind\":\"client_stream\",\"messages\":%d,\"iterations\":3,"
                      + "\"msgs_per_s\":%.1f}",
                  n,
                  median(rates)));
        }
      }
    }
    System.exit(0);
  }

  // ---- interop matrix client ----------------------------------------------

  /** The shared cases. Must match cases() in the Rust, Python and TS drivers. */
  static List<Map.Entry<String, EchoRequest>> cases() {
    byte[] zdata = new byte[20_000];
    for (int i = 0; i < zdata.length; i++) {
      zdata[i] = (byte) (i % 251);
    }
    return List.of(
        Map.entry("small", new EchoRequest("hello from java", 41, new byte[] {0, 1, 2, (byte) 255})),
        Map.entry("zstd", new EchoRequest("héllo ✓ ".repeat(200), -123_456, zdata)),
        Map.entry("long_string", new EchoRequest("x".repeat(5000), Integer.MAX_VALUE - 1, new byte[0])));
  }

  static String check(EchoRequest req, EchoReply reply) {
    if (!req.message.equals(reply.message)) {
      throw new IllegalStateException(
          "message mismatch (len " + reply.message.length() + " vs " + req.message.length() + ")");
    }
    if (reply.count != req.count + 1) {
      throw new IllegalStateException("count " + reply.count + " != " + (req.count + 1));
    }
    if (!Arrays.equals(req.data, reply.data)) {
      throw new IllegalStateException(
          "data mismatch (len " + reply.data.length + " vs " + req.data.length + ")");
    }
    if (reply.server_lang == null || reply.server_lang.isEmpty()) {
      throw new IllegalStateException("empty server_lang");
    }
    return reply.server_lang;
  }

  interface Case {
    String run() throws Exception;
  }

  static boolean runCase(String name, Case c) {
    try {
      System.out.println("CASE " + name + " OK server_lang=" + c.run());
      return true;
    } catch (Exception e) {
      Throwable t = e instanceof java.util.concurrent.ExecutionException && e.getCause() != null
          ? e.getCause()
          : e;
      System.out.println("CASE " + name + " FAIL " + t.getClass().getSimpleName() + ": " + t.getMessage());
      return false;
    } finally {
      System.out.flush();
    }
  }

  static void client(String ticket) throws Exception {
    ForyCodec codec = new ForyCodec();
    registerTypes(codec.fory());
    NodeAddr addr = NodeAddr.fromTicket(ticket);
    boolean ok = true;
    // Open gate, aster/1 directly (like the other languages' matrix clients): a Rust
    // AsterServer serves no consumer-admission ALPN.
    try (AsterClient client =
        AsterClient.builder()
            .config(config())
            .codec(codec)
            .consumerAdmission(false)
            .build()
            .get(30, TimeUnit.SECONDS)) {
      for (Map.Entry<String, EchoRequest> e : cases()) {
        EchoRequest req = e.getValue();
        ok &=
            runCase(
                e.getKey(),
                () ->
                    check(
                        req,
                        client
                            .<EchoRequest, EchoReply>call(
                                addr, "InteropEcho", "echo", req, EchoReply.class)
                            .get(15, TimeUnit.SECONDS)));
      }
      ok &=
          runCase(
              "server_stream",
              () -> {
                EchoRequest req = new EchoRequest("stream", 3, new byte[] {7, 8, 9});
                List<EchoReply> got =
                    client
                        .<EchoRequest, EchoReply>callServerStream(
                            addr, "InteropEcho", "generate", req, EchoReply.class)
                        .get(15, TimeUnit.SECONDS);
                if (got.size() != 3) {
                  throw new IllegalStateException(got.size() + " replies != 3");
                }
                for (int i = 0; i < 3; i++) {
                  EchoReply r = got.get(i);
                  if (r.count != i || !r.message.equals(req.message) || !Arrays.equals(r.data, req.data)) {
                    throw new IllegalStateException("reply " + i + " mismatch");
                  }
                }
                return got.get(0).server_lang;
              });
      ok &=
          runCase(
              "client_stream",
              () -> {
                List<EchoRequest> reqs = new ArrayList<>();
                for (int i = 0; i < 3; i++) {
                  reqs.add(new EchoRequest("m" + i, i, new byte[] {(byte) i}));
                }
                EchoReply r =
                    client
                        .<EchoRequest, EchoReply>callClientStream(
                            addr, "InteropEcho", "ingest", reqs, EchoReply.class)
                        .get(15, TimeUnit.SECONDS);
                if (r.count != 3 || !r.message.equals("m2")) {
                  throw new IllegalStateException("reply mismatch: " + r.count + " " + r.message);
                }
                if (r.server_lang == null || r.server_lang.isEmpty()) {
                  throw new IllegalStateException("empty server_lang");
                }
                return r.server_lang;
              });
      ok &= runCase("bidi", () -> chatCase(client, addr));
      ok &= runCase("deadline", () -> deadlineCase(client, addr, codec));
      ok &= runCase("cancel", () -> cancelCase(client, addr));
      ok &= runCase("session", () -> sessionCase(client, addr));
      ok &= runCase("trace", () -> traceCase(client, addr));
    }
    System.exit(ok ? 0 : 1);
  }

  static final String TRACE_ID = "4bf92f3577b34da6a3ce929d0e0e4736";

  /**
   * {@code traceEcho} under a known, sampled {@code traceparent}: the client call takes the current
   * {@code Tracing} context, and the server must report the same trace id.
   */
  static String traceCase(AsterClient client, NodeAddr addr) throws Exception {
    byte[] span = new byte[8];
    new java.util.Random().nextBytes(span);
    span[0] |= 1;
    String traceparent = "00-" + TRACE_ID + "-" + java.util.HexFormat.of().formatHex(span) + "-01";
    EchoReply r =
        site.aster.otel.Tracing.withTraceContext(
            traceparent,
            Map.of(),
            () ->
                client
                    .<EchoRequest, EchoReply>call(
                        addr, "InteropEcho", "traceEcho", new EchoRequest("probe", 0, new byte[0]), EchoReply.class)
                    .get(15, TimeUnit.SECONDS));
    if (!TRACE_ID.equals(r.message)) {
      throw new IllegalStateException("server saw trace id '" + r.message + "', want " + TRACE_ID);
    }
    return r.server_lang;
  }

  static int cancelledCount(AsterClient client, NodeAddr addr) throws Exception {
    return client
        .<EchoRequest, EchoReply>call(
            addr, "InteropEcho", "cancelled", new EchoRequest("probe", 0, new byte[0]), EchoReply.class)
        .get(10, TimeUnit.SECONDS)
        .count;
  }

  static void expectServerCancel(AsterClient client, NodeAddr addr, int want) throws Exception {
    int seen = 0;
    for (int i = 0; i < 50; i++) {
      seen = cancelledCount(client, addr);
      if (seen >= want) {
        return;
      }
      Thread.sleep(100);
    }
    throw new IllegalStateException(
        "server never observed the cancellation (count " + seen + ", want " + want + ")");
  }

  static String chatCase(AsterClient client, NodeAddr addr) throws Exception {
    String lang = "";
    try (BidiCall<EchoRequest, EchoReply> call =
        client.<EchoRequest, EchoReply>openBidiStream(addr, "InteropEcho", "chat", EchoReply.class)
            .get(15, TimeUnit.SECONDS)) {
      for (int i = 0; i < 3; i++) {
        EchoRequest req = new EchoRequest("c" + i, 100 + i, new byte[] {(byte) i, 9});
        call.send(req);
        EchoReply r = call.recv();
        if (r == null) {
          throw new IllegalStateException("stream ended before reply " + i);
        }
        if (r.count != i || !r.message.equals(req.message) || !Arrays.equals(r.data, req.data)) {
          throw new IllegalStateException("reply " + i + " mismatch");
        }
        lang = r.server_lang;
      }
      call.complete();
      if (call.recv() != null) {
        throw new IllegalStateException("unexpected extra reply");
      }
    }
    if (lang.isEmpty()) {
      throw new IllegalStateException("empty server_lang");
    }
    return lang;
  }

  /** {@code hold} with a 1 s deadline: DEADLINE_EXCEEDED, and the server observes it. */
  static String deadlineCase(AsterClient client, NodeAddr addr, ForyCodec codec) throws Exception {
    int before = cancelledCount(client, addr);
    IrohConnection conn = client.connect(addr).get(15, TimeUnit.SECONDS);
    Rpc.UnaryResult res =
        Rpc.unary(
            conn.runtime().nativeHandle(),
            conn.nativeHandle(),
            new Rpc.Options("InteropEcho", "hold", 1, 0, 1000, Map.of()),
            codec.encode(new EchoRequest("hold", 0, new byte[0])));
    if (res.code() != StatusCode.DEADLINE_EXCEEDED.value()) {
      throw new IllegalStateException("expected DEADLINE_EXCEEDED, got code " + res.code());
    }
    expectServerCancel(client, addr, before + 1);
    return "java";
  }

  /** {@code hold} cancelled by the client after 300 ms: the server observes it. */
  static String cancelCase(AsterClient client, NodeAddr addr) throws Exception {
    int before = cancelledCount(client, addr);
    try (ServerStreamCall<EchoReply> call =
        client
            .<EchoRequest, EchoReply>openServerStream(
                addr, "InteropEcho", "hold", new EchoRequest("hold", 0, new byte[0]), EchoReply.class)
            .get(15, TimeUnit.SECONDS)) {
      Thread.sleep(300);
      call.cancel();
      try {
        EchoReply r = call.recv();
        throw new IllegalStateException("hold returned " + r + " instead of CANCELLED");
      } catch (RpcError e) {
        if (e.code() != StatusCode.CANCELLED) {
          throw new IllegalStateException("expected CANCELLED, got " + e.code());
        }
      }
    }
    expectServerCancel(client, addr, before + 1);
    return "java";
  }

  /** Two sessions: state persists per session and is isolated between them. */
  static String sessionCase(AsterClient client, NodeAddr addr) throws Exception {
    ClientSession a = client.openSession(addr).get(15, TimeUnit.SECONDS);
    ClientSession b = client.openSession(addr).get(15, TimeUnit.SECONDS);
    Object[][] steps = {
      {a, 5, 5, "calls=1"}, {a, 7, 12, "calls=2"}, {b, 1, 1, "calls=1"},
      {a, 0, 12, "calls=3"}, {b, 10, 11, "calls=2"},
    };
    String lang = "";
    for (int i = 0; i < steps.length; i++) {
      ClientSession s = (ClientSession) steps[i][0];
      EchoReply r =
          s.<EchoRequest, EchoReply>call(
                  "InteropSession",
                  "add",
                  new EchoRequest("add", (Integer) steps[i][1], new byte[0]),
                  EchoReply.class)
              .get(15, TimeUnit.SECONDS);
      if (r.count != (Integer) steps[i][2] || !r.message.equals(steps[i][3])) {
        throw new IllegalStateException(
            "step " + i + ": got total " + r.count + " " + r.message + ", want total "
                + steps[i][2] + " " + steps[i][3]);
      }
      lang = r.server_lang;
    }
    return lang;
  }

  // ---- expose mini-matrix (server only) -----------------------------------

  static final byte[] EXPOSE_ALPN = "aster-expose/interop/1".getBytes(java.nio.charset.StandardCharsets.UTF_8);

  static void exposeServe() throws Exception {
    IrohNode node = IrohNode.start(config(), List.of(EXPOSE_ALPN), null).get(30, TimeUnit.SECONDS);
    Expose expose = Expose.create(node);
    expose.exposeHttp(
        "interop-http",
        req -> {
          if (req.method().equals("GET") && req.uri().equals("/hello")) {
            return HttpResponse.text(200, "hello from java").header("x-server-lang", "java");
          }
          if (req.method().equals("POST") && req.uri().equals("/echo")) {
            return HttpResponse.streaming(
                    200,
                    out -> {
                      byte[] buf = new byte[8192];
                      int n;
                      while ((n = req.body().read(buf)) >= 0) {
                        out.write(buf, 0, n);
                      }
                    })
                .header("x-server-lang", "java");
          }
          return HttpResponse.of(404, new byte[0]).header("x-server-lang", "java");
        },
        null);
    expose.serve(List.of(EXPOSE_ALPN));
    NodeAddr addr = node.nodeAddr();
    for (int i = 0; i < 100 && addr.directAddresses().isEmpty(); i++) {
      Thread.sleep(50);
      addr = node.nodeAddr();
    }
    System.out.println("ADDRESS " + addr.toTicket());
    System.out.flush();
    BufferedReader stdin = new BufferedReader(new InputStreamReader(System.in));
    while (stdin.readLine() != null) {
      // serve until stdin closes
    }
    expose.close();
    node.close();
    System.exit(0);
  }

  public static void main(String[] args) throws Exception {
    System.out.println("CONTRACT_ID " + ContractManifestBuilder.computeContractId(new Dispatcher()));
    System.out.println(
        "SESSION_CONTRACT_ID " + ContractManifestBuilder.computeContractId(new SessionDispatcher()));
    System.out.flush();
    if (args.length == 1 && args[0].equals("contract")) {
      System.exit(0);
    } else if (args.length >= 1 && args[0].equals("server")) {
      serve();
    } else if (args.length == 1 && args[0].equals("expose-server")) {
      exposeServe();
    } else if (args.length == 2 && args[0].equals("client")) {
      client(args[1]);
    } else if (args.length >= 2 && args[0].equals("bench")) {
      bench(args);
    } else {
      System.err.println(
          "usage: InteropEcho.java contract | server | client <aster1-ticket> | bench <aster1-ticket> [options]");
      System.exit(2);
    }
  }
}
