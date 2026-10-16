package site.aster.blobs;

import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

/**
 * Node-wide blob transfer counters since the node started ({@code aster::BlobTransferStats}, C ABI
 * 7.4 {@code aster_blob_transfer_stats_t}). Monotonic. Payload is blob content as decoded, whatever
 * the encoding: compression coverage is {@code *ZstdPayloadBytes / *PayloadBytes}, its saving
 * {@code *PlainBytes - *WireBytes}.
 *
 * @param fetchedPayloadBytes payload bytes fetched, plain and zstd
 * @param fetchedZstdPayloadBytes the part of {@code fetchedPayloadBytes} that came over zstd
 * @param fetchedWireBytes compressed bytes received while fetching over zstd
 * @param fetchedPlainBytes protocol bytes those decompressed to
 * @param servedPayloadBytes payload bytes served, plain and zstd
 * @param servedZstdPayloadBytes the part of {@code servedPayloadBytes} that went out over zstd
 * @param servedWireBytes compressed bytes sent while serving over zstd
 * @param servedPlainBytes protocol bytes those were compressed from
 * @param plainFallbacks fetch attempts that fell back to plain (the provider has no zstd)
 * @param plainServes requests served over plain transport
 * @param fetchPlainOk fetch attempts over plain that succeeded
 * @param fetchPlainFailed fetch attempts over plain that failed
 * @param fetchZstdOk fetch attempts over zstd that succeeded
 * @param fetchZstdFailed fetch attempts over zstd that failed
 * @param servePlainOk plain requests served successfully
 * @param servePlainFailed plain requests that failed
 * @param serveZstdOk zstd requests served successfully
 * @param serveZstdFailed zstd requests that failed
 * @param fetchDecodeErrors fetched zstd streams that failed on malformed zstd data
 * @param serveDecodeErrors served zstd streams that failed on malformed zstd data
 */
public record BlobTransferStats(
    long fetchedPayloadBytes,
    long fetchedZstdPayloadBytes,
    long fetchedWireBytes,
    long fetchedPlainBytes,
    long servedPayloadBytes,
    long servedZstdPayloadBytes,
    long servedWireBytes,
    long servedPlainBytes,
    long plainFallbacks,
    long plainServes,
    long fetchPlainOk,
    long fetchPlainFailed,
    long fetchZstdOk,
    long fetchZstdFailed,
    long servePlainOk,
    long servePlainFailed,
    long serveZstdOk,
    long serveZstdFailed,
    long fetchDecodeErrors,
    long serveDecodeErrors) {

  /** {@code sizeof(aster_blob_transfer_stats_t)}. */
  public static final long SIZE = 168;

  /** Decode a filled {@code aster_blob_transfer_stats_t} (20 u64 counters after an 8-byte head). */
  static BlobTransferStats fromNative(MemorySegment s) {
    long[] v = new long[20];
    for (int i = 0; i < v.length; i++) {
      v[i] = s.get(ValueLayout.JAVA_LONG, 8L + 8L * i);
    }
    return new BlobTransferStats(
        v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9], v[10], v[11], v[12], v[13],
        v[14], v[15], v[16], v[17], v[18], v[19]);
  }
}
