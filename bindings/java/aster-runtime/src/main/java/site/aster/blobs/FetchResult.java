package site.aster.blobs;

import java.util.Arrays;

/**
 * The bytes of a fetched blob (empty for a store-only fetch) and the {@link FetchReport}.
 *
 * @param data the blob bytes; empty when fetched to the store only
 * @param report what each provider did
 */
public record FetchResult(byte[] data, FetchReport report) {

  public FetchResult {
    data = data.clone();
  }

  @Override
  public byte[] data() {
    return data.clone();
  }

  /** Split an {@code ASTER_EVENT_BLOB_FETCHED} payload into report and blob bytes. */
  public static FetchResult parse(byte[] payload) {
    int end = FetchReport.encodedLength(payload);
    return new FetchResult(
        Arrays.copyOfRange(payload, end, payload.length), FetchReport.parse(payload));
  }

  @Override
  public boolean equals(Object o) {
    return o instanceof FetchResult r && Arrays.equals(data, r.data) && report.equals(r.report);
  }

  @Override
  public int hashCode() {
    return 31 * Arrays.hashCode(data) + report.hashCode();
  }

  @Override
  public String toString() {
    return "FetchResult{" + data.length + " bytes, " + report + "}";
  }
}
