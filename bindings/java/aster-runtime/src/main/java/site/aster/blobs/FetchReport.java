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

  /** Version byte of the C ABI fetch-report encoding this parser reads. */
  public static final int VERSION = 1;

  static final int HEADER_SIZE = 16;
  static final int RECORD_SIZE = 44;

  /** What one provider did during a fetch. */
  public record ProviderOutcome(String nodeId, int attempts, int failures, int completedRequests) {

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

  /** Size in bytes of the encoded report at the start of {@code payload}. */
  static int encodedLength(byte[] payload) {
    if (payload.length < HEADER_SIZE) {
      throw new IllegalArgumentException("fetch report shorter than its header");
    }
    int n = ByteBuffer.wrap(payload, 4, 4).order(ByteOrder.LITTLE_ENDIAN).getInt();
    long end = HEADER_SIZE + (long) n * RECORD_SIZE;
    if (n < 0 || end > payload.length) {
      throw new IllegalArgumentException("fetch report truncated");
    }
    return (int) end;
  }

  /**
   * Decode the C ABI fetch report v1 at the start of {@code payload} (see {@code
   * ASTER_FETCH_REPORT_VERSION} in {@code aster_ffi.h}).
   */
  public static FetchReport parse(byte[] payload) {
    int end = encodedLength(payload);
    ByteBuffer b = ByteBuffer.wrap(payload, 0, end).order(ByteOrder.LITTLE_ENDIAN);
    int version = Byte.toUnsignedInt(b.get(0));
    if (version != VERSION) {
      throw new IllegalArgumentException("unsupported fetch report version " + version);
    }
    int n = b.getInt(4);
    long transferred = b.getLong(8);
    HexFormat hex = HexFormat.of();
    List<ProviderOutcome> out = new ArrayList<>(n);
    for (int i = 0; i < n; i++) {
      int o = HEADER_SIZE + i * RECORD_SIZE;
      String id = hex.formatHex(payload, o, o + 32);
      out.add(new ProviderOutcome(id, b.getInt(o + 32), b.getInt(o + 36), b.getInt(o + 40)));
    }
    return new FetchReport(out, transferred);
  }
}
