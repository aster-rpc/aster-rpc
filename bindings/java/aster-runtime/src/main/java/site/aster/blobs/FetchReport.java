package site.aster.blobs;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;

/**
 * What a multi-provider fetch did ({@code aster::FetchReport}), for callers that rank providers.
 *
 * @param providers per-provider outcomes in first-attempt order; repeated use of one provider is
 *     aggregated
 * @param bytesTransferred payload bytes pulled off the wire (a transfer metric, not a content size)
 */
public record FetchReport(List<ProviderOutcome> providers, long bytesTransferred) {

  /** Version byte of the C ABI fetch report v1 ({@code ASTER_FETCH_REPORT_VERSION}). */
  public static final int VERSION = 1;

  /** Version byte of fetch report v2 ({@code ASTER_FETCH_REPORT_VERSION_2}, ABI 7.4). */
  public static final int VERSION_2 = 2;

  static final int HEADER_SIZE = 16;
  static final int RECORD_SIZE = 44;
  static final int RECORD_V2_SIZE = 72;

  /** {@code ASTER_FETCH_PROVIDER_HAS_WIRE_BYTES}: the v2 record's {@code wire_bytes} is set. */
  static final int HAS_WIRE_BYTES = 1;

  /**
   * What one provider did during a fetch ({@code aster::ProviderOutcome}).
   *
   * @param inFlight requests still running against this provider when the fetch ended (v2)
   * @param plainFallbacks attempts that went over plain transport because this provider serves no
   *     compressed blobs: a capability, not a content outcome (v2)
   * @param bytesTransferred payload bytes this provider carried, failed attempts included (v2)
   * @param wireBytes compressed bytes received from this provider over zstd; {@code null} when no
   *     attempt against it went over zstd (v2)
   */
  public record ProviderOutcome(
      String nodeId,
      int attempts,
      int failures,
      int completedRequests,
      int inFlight,
      int plainFallbacks,
      long bytesTransferred,
      Long wireBytes) {

    /** A v1 outcome (no per-provider transfer counters). */
    public ProviderOutcome(String nodeId, int attempts, int failures, int completedRequests) {
      this(nodeId, attempts, failures, completedRequests, 0, 0, 0L, null);
    }

    /** Whether at least one request against this provider failed. */
    public boolean failed() {
      return failures > 0;
    }

    /** Whether this provider carried at least one request to completion. */
    public boolean served() {
      return completedRequests > 0;
    }
  }

  public FetchReport {
    providers = List.copyOf(providers);
  }

  /** Providers that were dialled, in first-attempt order. */
  public List<String> attempted() {
    return providers.stream().map(ProviderOutcome::nodeId).toList();
  }

  /** Providers where at least one request failed. */
  public List<String> failed() {
    return providers.stream().filter(ProviderOutcome::failed).map(ProviderOutcome::nodeId).toList();
  }

  /** Providers that carried at least one request to completion. */
  public List<String> served() {
    return providers.stream().filter(ProviderOutcome::served).map(ProviderOutcome::nodeId).toList();
  }

  /** Providers with requests still running when the fetch ended (v2). */
  public List<String> inFlight() {
    return providers.stream().filter(p -> p.inFlight() > 0).map(ProviderOutcome::nodeId).toList();
  }

  /**
   * Providers that serve no compressed blobs and were fetched from (or tried) over plain transport
   * instead (v2). Cache these as plain-only rather than ranking them down.
   */
  public List<String> plainOnly() {
    return providers.stream()
        .filter(p -> p.plainFallbacks() > 0)
        .map(ProviderOutcome::nodeId)
        .toList();
  }

  private static int recordSize(int version) {
    return switch (version) {
      case VERSION -> RECORD_SIZE;
      case VERSION_2 -> RECORD_V2_SIZE;
      default -> throw new IllegalArgumentException("unsupported fetch report version " + version);
    };
  }

  /** Size in bytes of the encoded report (v1 or v2) at the start of {@code payload}. */
  static int encodedLength(byte[] payload) {
    if (payload.length < HEADER_SIZE) {
      throw new IllegalArgumentException("fetch report shorter than its header");
    }
    int record = recordSize(Byte.toUnsignedInt(payload[0]));
    long n =
        Integer.toUnsignedLong(
            ByteBuffer.wrap(payload, 4, 4).order(ByteOrder.LITTLE_ENDIAN).getInt());
    long end = HEADER_SIZE + n * record;
    if (end > payload.length) {
      throw new IllegalArgumentException("fetch report truncated");
    }
    return (int) end;
  }

  /**
   * Decode the C ABI fetch report (v1, or v2 with {@code ASTER_FETCH_REPORT_V2}) at the start of
   * {@code payload} (see {@code ASTER_FETCH_REPORT_VERSION} / {@code _VERSION_2} in {@code
   * aster_ffi.h}).
   */
  public static FetchReport parse(byte[] payload) {
    int end = encodedLength(payload);
    ByteBuffer b = ByteBuffer.wrap(payload, 0, end).order(ByteOrder.LITTLE_ENDIAN);
    int version = Byte.toUnsignedInt(b.get(0));
    int record = recordSize(version);
    int n = b.getInt(4);
    long transferred = b.getLong(8);
    HexFormat hex = HexFormat.of();
    List<ProviderOutcome> out = new ArrayList<>(n);
    for (int i = 0; i < n; i++) {
      int o = HEADER_SIZE + i * record;
      String id = hex.formatHex(payload, o, o + 32);
      int attempts = b.getInt(o + 32);
      int failures = b.getInt(o + 36);
      int completed = b.getInt(o + 40);
      if (version == VERSION) {
        out.add(new ProviderOutcome(id, attempts, failures, completed));
        continue;
      }
      int flags = b.getInt(o + 52);
      out.add(
          new ProviderOutcome(
              id,
              attempts,
              failures,
              completed,
              b.getInt(o + 44),
              b.getInt(o + 48),
              b.getLong(o + 56),
              (flags & HAS_WIRE_BYTES) != 0 ? b.getLong(o + 64) : null));
    }
    return new FetchReport(out, transferred);
  }
}
