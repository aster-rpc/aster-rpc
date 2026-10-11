package site.aster.topology;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.lang.foreign.Arena;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.math.BigInteger;
import java.time.Duration;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.NativeCall;
import site.aster.node.IrohNode;
import site.aster.upcall.AdmittedFilterHandle;
import site.aster.upcall.UpcallNative;

/** Local proximity and shared topology views. Monitoring is required for measurements. */
public final class Topology {
  static final ObjectMapper JSON = new ObjectMapper();
  private final IrohNode node;

  public Topology(IrohNode node) {
    this.node = java.util.Objects.requireNonNull(node);
  }

  /** Locality ladder, ordered from nearest to farthest. */
  public enum LadderLevel {
    SAME_HOST,
    SAME_LAN,
    SAME_SITE,
    SAME_REGION,
    FAR
  }

  /** Absence of evidence is UNKNOWN, never evidence of separation. */
  public enum SeparationVerdict {
    UNKNOWN,
    CONNECTED,
    SEPARATED
  }

  /**
   * One local peer measurement. Nullable metrics have not yet been measured. Native unsigned 64-bit
   * metrics use BigInteger.
   */
  public record PeerView(
      String nodeId,
      LadderLevel level,
      String levelReason,
      BigInteger rttUs,
      BigInteger jitterUs,
      long lossPpm,
      BigInteger throughputBps,
      BigInteger cwndCeilingBps,
      boolean direct,
      String relayUrl,
      String remoteAddr,
      BigInteger congestionEvents,
      BigInteger samples,
      long confidencePpm,
      BigInteger lastMeasuredUnixMs,
      BigInteger clusterHeldSinceUnixMs,
      boolean isConnected) {}

  /** Members and witnesses are immutable, with native ordering preserved. */
  public record ClusterView(List<String> members, String bridge, List<String> witnesses) {
    public ClusterView {
      members = List.copyOf(members);
      witnesses = List.copyOf(witnesses);
    }
  }

  /** Optional reader rules; null uses native defaults. Explicit zero is retained. */
  public record Timing(
      Duration minHold,
      Duration livenessTtl,
      Duration edgeTtl,
      Duration maxSkew,
      Long separationCoverage,
      Long witnessR) {
    public Timing {
      millis(minHold, "minHold");
      millis(livenessTtl, "livenessTtl");
      millis(edgeTtl, "edgeTtl");
      millis(maxSkew, "maxSkew");
      u32(separationCoverage, "separationCoverage");
      u32(witnessR, "witnessR");
    }
  }

  /** Read-time admission is native-cached; a null filter counts every namespace-secret holder. */
  public record SwarmOptions(
      List<String> bootstrap,
      AdmittedFilterHandle admitted,
      Duration positionRefresh,
      Timing timing) {
    public SwarmOptions {
      bootstrap = bootstrap == null ? List.of() : List.copyOf(bootstrap);
      millis(positionRefresh, "positionRefresh");
    }

    public static SwarmOptions defaults() {
      return new SwarmOptions(List.of(), null, null, null);
    }
  }

  static MethodHandle handle(String name, java.lang.foreign.MemoryLayout... args) {
    return NativeCall.handle(name, NativeCall.intFn(args));
  }

  private static final ValueLayout.OfLong J = ValueLayout.JAVA_LONG;
  private static final java.lang.foreign.AddressLayout A = ValueLayout.ADDRESS;
  private static final MethodHandle MONITORING = handle("aster_topology_has_monitoring", J, J, A);
  private static final MethodHandle PEERS = handle("aster_topology_peers", J, J, A);
  private static final MethodHandle PEER = handle("aster_topology_peer", J, J, NativeCall.BYTES, A);
  private static final MethodHandle SNAPSHOT = handle("aster_topology_snapshot", J, J, J, A);
  private static final MethodHandle ENABLE =
      handle(
          "aster_topology_enable_shared_with_options",
          J,
          J,
          A,
          J,
          site.aster.ffi.IrohLibrary.ASTER_BYTES_LIST,
          NativeCall.BYTES,
          J,
          A);

  /** False when monitoring was disabled at node creation; its local peer view is then empty. */
  public boolean hasMonitoring() {
    try (Arena a = Arena.ofConfined()) {
      var out = a.allocate(ValueLayout.JAVA_INT);
      NativeCall.callChecked(
          MONITORING,
          "topology_has_monitoring",
          node.runtime().nativeHandle(),
          node.nodeHandle(),
          out);
      return out.get(ValueLayout.JAVA_INT, 0) != 0;
    }
  }

  /** Every known peer, including recently disconnected peers with decaying confidence. */
  public List<PeerView> peers() {
    JsonNode values =
        json(PEERS, "topology_peers", node.runtime().nativeHandle(), node.nodeHandle());
    var result = new ArrayList<PeerView>();
    values.forEach(v -> result.add(peer(v)));
    return List.copyOf(result);
  }

  /** Empty for an unknown or unmeasured peer. */
  public Optional<PeerView> peer(String nodeId) {
    try (Arena a = Arena.ofConfined()) {
      var value =
          json(
              PEER,
              "topology_peer",
              node.runtime().nativeHandle(),
              node.nodeHandle(),
              NativeCall.string(a, nodeId));
      return value.isNull() ? Optional.empty() : Optional.of(peer(value));
    }
  }

  /**
   * Join a shared swarm. Zero position refresh means 60 seconds; other zero rules are retained.
   * Canceling the returned wait does not roll back a join: the node owns its publisher until close
   * or rejoin, and a native join already in progress may finish.
   */
  public CompletableFuture<Void> enableSharedAsync(byte[] secret, SwarmOptions options) {
    if (secret == null || secret.length != 32)
      throw new IllegalArgumentException("secret must be 32 bytes");
    byte[] key = secret.clone();
    var opts = options == null ? SwarmOptions.defaults() : options;
    Map<String, Long> fields = new LinkedHashMap<>();
    put(fields, "position_refresh_ms", millis(opts.positionRefresh(), "positionRefresh"));
    if (opts.timing() != null) {
      var t = opts.timing();
      put(fields, "min_hold_ms", millis(t.minHold(), "minHold"));
      put(fields, "liveness_ttl_ms", millis(t.livenessTtl(), "livenessTtl"));
      put(fields, "edge_ttl_ms", millis(t.edgeTtl(), "edgeTtl"));
      put(fields, "max_skew_ms", millis(t.maxSkew(), "maxSkew"));
      put(fields, "separation_coverage", t.separationCoverage());
      put(fields, "witness_r", t.witnessR());
    }
    final byte[] encoded;
    try {
      encoded = JSON.writeValueAsBytes(fields);
    } catch (Exception e) {
      throw new IllegalArgumentException(e);
    }
    return UpcallNative.submit(
            node.runtime(),
            "topology_enable_shared_with_options",
            (a, op) ->
                NativeCall.call(
                    ENABLE,
                    node.runtime().nativeHandle(),
                    node.nodeHandle(),
                    NativeCall.key(a, key, "secret"),
                    opts.admitted() == null ? 0L : opts.admitted().nativeHandle(),
                    DataPlane.utf8List(a, opts.bootstrap()),
                    NativeCall.bytes(a, encoded),
                    0L,
                    op))
        .thenApply(r -> null);
  }

  /**
   * Capture one immutable native view. Close the returned owner when finished. Every snapshot query
   * uses the same view; it is local evidence, not an authorization proof. Canceling this future
   * disposes a late snapshot rather than abandoning its native owner.
   */
  public CompletableFuture<TopologySnapshot> snapshotAsync() {
    var result = new CompletableFuture<TopologySnapshot>();
    UpcallNative.submit(
            node.runtime(),
            "topology_snapshot",
            (a, op) ->
                NativeCall.call(SNAPSHOT, node.runtime().nativeHandle(), node.nodeHandle(), 0L, op))
        .whenComplete(
            (r, error) -> {
              if (error != null) {
                result.completeExceptionally(error);
                return;
              }
              try {
                if (r.event().kind() != IrohEventKind.TOPOLOGY_SNAPSHOT)
                  throw new IllegalStateException("unexpected topology snapshot event");
                var owner = new TopologySnapshot(node.runtime(), r.event().handle());
                if (!result.complete(owner)) owner.close();
              } catch (Throwable e) {
                result.completeExceptionally(e);
              }
            });
    return result;
  }

  /** Convenience fresh observation; use snapshotAsync for several consistent decisions. */
  public CompletableFuture<List<ClusterView>> clustersAsync() {
    return observe(TopologySnapshot::clusters);
  }

  /** Convenience fresh observation. */
  public CompletableFuture<Optional<ClusterView>> myClusterAsync() {
    String self = node.nodeId();
    return observe(s -> s.clusterOf(self));
  }

  /** Convenience fresh observation. */
  public CompletableFuture<SeparationVerdict> separatedAsync(String a, String b) {
    return observe(s -> s.separated(a, b));
  }

  private <T> CompletableFuture<T> observe(java.util.function.Function<TopologySnapshot, T> query) {
    var result = new CompletableFuture<T>();
    snapshotAsync()
        .whenComplete(
            (snapshot, error) -> {
              if (error != null) {
                result.completeExceptionally(error);
                return;
              }
              try (snapshot) {
                result.complete(query.apply(snapshot));
              } catch (Throwable t) {
                result.completeExceptionally(t);
              }
            });
    return result;
  }

  static JsonNode json(MethodHandle h, String name, Object... args) {
    try (Arena a = Arena.ofConfined()) {
      var out = a.allocate(NativeCall.BYTES);
      Object[] all = java.util.Arrays.copyOf(args, args.length + 1);
      all[args.length] = out;
      int rc = NativeCall.call(h, all);
      byte[] bytes = NativeCall.takeOwned(out);
      NativeCall.check(rc, name);
      return JSON.readTree(bytes);
    } catch (java.io.IOException e) {
      throw new IllegalStateException("invalid native topology JSON", e);
    }
  }

  static ClusterView cluster(JsonNode v) {
    return new ClusterView(
        strings(v.get("members")), v.get("bridge").asText(), strings(v.get("witnesses")));
  }

  static List<String> strings(JsonNode v) {
    var values = new ArrayList<String>();
    v.forEach(n -> values.add(n.asText()));
    return List.copyOf(values);
  }

  static PeerView peer(JsonNode v) {
    return new PeerView(
        v.get("node_id").asText(),
        LadderLevel.values()[v.get("level").asInt()],
        v.get("level_reason").asText(),
        nullableUnsigned(v.get("rtt_us")),
        nullableUnsigned(v.get("jitter_us")),
        v.get("loss_ppm").asLong(),
        unsigned(v.get("throughput_bps")),
        unsigned(v.get("cwnd_ceiling_bps")),
        v.get("direct").asBoolean(),
        nullableString(v.get("relay_url")),
        nullableString(v.get("remote_addr")),
        unsigned(v.get("congestion_events")),
        unsigned(v.get("samples")),
        v.get("confidence_ppm").asLong(),
        unsigned(v.get("last_measured_unix_ms")),
        unsigned(v.get("cluster_held_since_unix_ms")),
        v.get("is_connected").asBoolean());
  }

  private static BigInteger nullableUnsigned(JsonNode v) {
    return v == null || v.isNull() ? null : unsigned(v);
  }

  private static BigInteger unsigned(JsonNode v) {
    BigInteger value = v.bigIntegerValue();
    if (!v.isIntegralNumber() || value.signum() < 0 || value.bitLength() > 64)
      throw new IllegalStateException("invalid native unsigned 64-bit metric");
    return value;
  }

  private static String nullableString(JsonNode v) {
    return v == null || v.isNull() ? null : v.asText();
  }

  private static void put(Map<String, Long> m, String k, Long v) {
    if (v != null) m.put(k, v);
  }

  private static Long millis(Duration value, String name) {
    if (value == null) return null;
    if (value.isNegative()) throw new IllegalArgumentException(name + " must be nonnegative");
    long ms;
    try {
      ms = value.toMillis();
    } catch (ArithmeticException e) {
      throw new IllegalArgumentException(name + " exceeds u32 milliseconds", e);
    }
    u32(ms, name);
    return ms;
  }

  private static void u32(Long value, String name) {
    if (value != null && (value < 0 || value > 0xffff_ffffL))
      throw new IllegalArgumentException(name + " must fit u32");
  }
}
