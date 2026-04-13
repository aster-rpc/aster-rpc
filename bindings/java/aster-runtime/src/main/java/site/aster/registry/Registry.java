package site.aster.registry;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.core.type.TypeReference;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.List;
import site.aster.ffi.IrohLibrary;

/**
 * High-level Java entry points into the Rust registry logic (§11).
 *
 * <p>All resolution filtering and ranking is performed in Rust — this class is a thin wrapper over
 * the synchronous FFI functions so callers construct {@link EndpointLease} objects in Java, pass
 * them through {@link #filterAndRank}, and get a ranked list back. Doc reads and writes still go
 * through the existing {@code IrohNode}/doc FFI.
 */
public final class Registry {

  private static final int INITIAL_OUT_BUF = 16 * 1024;
  private static final TypeReference<List<EndpointLease>> LEASE_LIST = new TypeReference<>() {};

  private Registry() {}

  /** Single shared wall-clock reading used by freshness checks across languages. */
  public static long nowEpochMs() {
    return IrohLibrary.getInstance().asterRegistryNowEpochMs();
  }

  /** Return true if the given lease is still within the freshness window. */
  public static boolean isFresh(EndpointLease lease, int leaseDurationS) {
    byte[] json = lease.toJson().getBytes(StandardCharsets.UTF_8);
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment seg = arena.allocate(json.length);
      seg.copyFrom(MemorySegment.ofArray(json));
      int result = IrohLibrary.getInstance().asterRegistryIsFresh(seg, json.length, leaseDurationS);
      if (result < 0) throw new IllegalStateException("aster_registry_is_fresh failed: " + result);
      return result == 1;
    }
  }

  /**
   * Apply the §11.9 mandatory filters and ranking strategy to a list of leases.
   *
   * <p>Returns the ranked survivors in best-first order. An empty list means no candidate passed
   * the filters.
   */
  public static List<EndpointLease> filterAndRank(List<EndpointLease> leases, ResolveOptions opts) {
    byte[] leasesBytes;
    byte[] optsBytes;
    try {
      leasesBytes = RegistryMapper.MAPPER.writeValueAsBytes(leases);
      optsBytes = RegistryMapper.MAPPER.writeValueAsBytes(opts);
    } catch (JsonProcessingException e) {
      throw new IllegalArgumentException("failed to encode filterAndRank inputs", e);
    }

    try (Arena arena = Arena.ofConfined()) {
      MemorySegment leasesSeg = arena.allocate(leasesBytes.length);
      leasesSeg.copyFrom(MemorySegment.ofArray(leasesBytes));
      MemorySegment optsSeg = arena.allocate(optsBytes.length);
      optsSeg.copyFrom(MemorySegment.ofArray(optsBytes));

      byte[] out = invokeFilterAndRank(leasesSeg, leasesBytes.length, optsSeg, optsBytes.length);
      try {
        return RegistryMapper.MAPPER.readValue(out, LEASE_LIST);
      } catch (java.io.IOException e) {
        throw new IllegalStateException("failed to decode ranked leases", e);
      }
    }
  }

  private static byte[] invokeFilterAndRank(
      MemorySegment leasesSeg, long leasesLen, MemorySegment optsSeg, long optsLen) {
    int cap = INITIAL_OUT_BUF;
    while (true) {
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment outBuf = arena.allocate(cap);
        MemorySegment outLen = arena.allocate(ValueLayout.JAVA_LONG);
        outLen.set(ValueLayout.JAVA_LONG, 0, cap);
        int status =
            IrohLibrary.getInstance()
                .asterRegistryFilterAndRank(leasesSeg, leasesLen, optsSeg, optsLen, outBuf, outLen);
        long written = outLen.get(ValueLayout.JAVA_LONG, 0);
        if (status == 0) {
          byte[] result = new byte[(int) written];
          MemorySegment.copy(outBuf, ValueLayout.JAVA_BYTE, 0, result, 0, (int) written);
          return result;
        }
        // Grow once on BUFFER_TOO_SMALL (written = required size).
        if (written > cap) {
          cap = (int) written;
          continue;
        }
        throw new IllegalStateException("aster_registry_filter_and_rank failed: " + status);
      }
    }
  }

  // ── Key helpers (delegating to Rust) ─────────────────────────────────────

  public static byte[] contractKeyRust(String contractId) {
    return callKey(0, contractId, "", "");
  }

  public static byte[] versionKeyRust(String name, int version) {
    return callKey(1, name, Integer.toString(version), "");
  }

  public static byte[] channelKeyRust(String name, String channel) {
    return callKey(2, name, channel, "");
  }

  public static byte[] leaseKeyRust(String name, String contractId, String endpointId) {
    return callKey(3, name, contractId, endpointId);
  }

  public static byte[] leasePrefixRust(String name, String contractId) {
    return callKey(4, name, contractId, "");
  }

  public static byte[] aclKeyRust(String subkey) {
    return callKey(5, subkey, "", "");
  }

  private static byte[] callKey(int kind, String a1, String a2, String a3) {
    byte[] b1 = a1.getBytes(StandardCharsets.UTF_8);
    byte[] b2 = a2.getBytes(StandardCharsets.UTF_8);
    byte[] b3 = a3.getBytes(StandardCharsets.UTF_8);
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment s1 = arena.allocate(Math.max(1, b1.length));
      if (b1.length > 0) s1.copyFrom(MemorySegment.ofArray(b1));
      MemorySegment s2 = arena.allocate(Math.max(1, b2.length));
      if (b2.length > 0) s2.copyFrom(MemorySegment.ofArray(b2));
      MemorySegment s3 = arena.allocate(Math.max(1, b3.length));
      if (b3.length > 0) s3.copyFrom(MemorySegment.ofArray(b3));
      int cap = 512;
      MemorySegment outBuf = arena.allocate(cap);
      MemorySegment outLen = arena.allocate(ValueLayout.JAVA_LONG);
      outLen.set(ValueLayout.JAVA_LONG, 0, cap);
      int status =
          IrohLibrary.getInstance()
              .asterRegistryKey(kind, s1, b1.length, s2, b2.length, s3, b3.length, outBuf, outLen);
      if (status != 0) {
        throw new IllegalStateException("aster_registry_key failed: " + status);
      }
      long written = outLen.get(ValueLayout.JAVA_LONG, 0);
      byte[] out = new byte[(int) written];
      MemorySegment.copy(outBuf, ValueLayout.JAVA_BYTE, 0, out, 0, (int) written);
      return out;
    }
  }

  /** Options controlling resolve filtering and ranking. Mirrors Rust ResolveOptions. */
  @JsonInclude(JsonInclude.Include.NON_NULL)
  public static final class ResolveOptions {
    @JsonProperty("service")
    public String service = "";

    @JsonProperty("version")
    public Integer version;

    @JsonProperty("channel")
    public String channel;

    @JsonProperty("contract_id")
    public String contractId;

    @JsonProperty("strategy")
    public String strategy = "round_robin";

    @JsonProperty("caller_alpn")
    public String callerAlpn = "aster/1";

    @JsonProperty("caller_serialization_modes")
    public List<String> callerSerializationModes = List.of("fory-xlang");

    @JsonProperty("caller_policy_realm")
    public String callerPolicyRealm;

    @JsonProperty("lease_duration_s")
    public int leaseDurationS = 45;
  }
}
