// The Java side of the InteropEcho contract (tests/interop/), used by the
// dispatch benchmark (benchmarks/dispatch/run.py). Not yet part of the
// tests/interop/run.py matrix (no contract-id/codegen leg for Java).
//
// Contract (must match tests/interop/rust/contract.rs):
//
//   service InteropEcho v1
//     echo(interop/EchoRequest) -> interop/EchoReply                (unary)
//     generate(interop/EchoRequest) -> stream interop/EchoReply     (server stream)
//     ingest(stream interop/EchoRequest) -> interop/EchoReply       (client stream)
//
// Single-file source program (JEP 330), no build step of its own:
//
//   java --enable-native-access=ALL-UNNAMED -cp <aster-runtime classpath> \
//       tests/interop/java/InteropEcho.java server
//   java ... InteropEcho.java bench <aster1-ticket> [--duration S] [--warmup S]
//       [--concurrency 1,10,50,100] [--stream-frames 100,10000] [--client-stream 10000]
//
// IROH_LIB_PATH must point at the release libaster_transport_ffi. `server`
// prints ADDRESS <aster1...> and serves until stdin closes; `bench` prints
// RESULT {json} lines in the schema of benchmarks/dispatch/driver.rs.

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
import java.util.concurrent.atomic.AtomicLong;
import org.apache.fory.BaseFory;
import org.apache.fory.Fory;
import site.aster.annotations.Scope;
import site.aster.client.AsterClient;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.codec.ForyTags;
import site.aster.config.AsterConfig;
import site.aster.interceptors.CallContext;
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

  /** interop/EchoReply { message, count, data, server_lang }. */
  public static final class EchoReply {
    public String message = "";
    public int count;
    public byte[] data = new byte[0];
    public String serverLang = "";

    public EchoReply() {}

    EchoReply(String message, int count, byte[] data, String serverLang) {
      this.message = message;
      this.count = count;
      this.data = data;
      this.serverLang = serverLang;
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
      return Map.of("echo", new Echo(), "generate", new Generate(), "ingest", new Ingest());
    }

    @Override
    public Map<String, Class<?>> requestClasses() {
      return Map.of(
          "echo", EchoRequest.class, "generate", EchoRequest.class, "ingest", EchoRequest.class);
    }

    @Override
    public Map<String, Class<?>> responseClasses() {
      return Map.of("echo", EchoReply.class, "ingest", EchoReply.class);
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
        AsterClient.builder().config(config()).codec(codec).build().get(30, TimeUnit.SECONDS)) {
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

  public static void main(String[] args) throws Exception {
    if (args.length >= 1 && args[0].equals("server")) {
      serve();
    } else if (args.length >= 2 && args[0].equals("bench")) {
      bench(args);
    } else {
      System.err.println("usage: InteropEcho.java server | bench <aster1-ticket> [options]");
      System.exit(2);
    }
  }
}
