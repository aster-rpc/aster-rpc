package site.aster.client;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.concurrent.CompletableFuture;
import java.util.function.LongFunction;
import site.aster.handle.IrohStream;

/**
 * Buffers raw bytes read from an {@link IrohStream} and yields complete Aster frames.
 *
 * <p>{@link IrohStream#readAsync(long)} returns raw QUIC bytes in unspecified chunks, not frames,
 * so any frame-aware consumer has to reassemble the {@code [4B LE len][1B flags][payload]} format
 * itself. This class owns that state: callers only see {@link #readFrame()} returning whole frames.
 *
 * <p>Mirrors {@code bindings/python/aster/framing.py#read_frame}. The Python reader relies on
 * {@code IrohRecvStream.read_exact(n)}, which Rust already buffers for it; Java has no equivalent,
 * hence the explicit accumulator here.
 */
public final class ClientFrameReader {

  public static final int MAX_FRAME_SIZE = 16 * 1024 * 1024;
  private static final long CHUNK_SIZE = 65536L;

  private final LongFunction<CompletableFuture<byte[]>> chunkReader;
  private byte[] buffer = new byte[0];
  private int bufferLen = 0;

  public ClientFrameReader(IrohStream stream) {
    this(stream::readAsync);
  }

  /**
   * Construct with an explicit chunk reader. Primarily for unit tests that stub the chunk delivery
   * sequence without a real QUIC stream.
   */
  public ClientFrameReader(LongFunction<CompletableFuture<byte[]>> chunkReader) {
    this.chunkReader = chunkReader;
  }

  /** A complete frame: the payload bytes and the 1-byte flags value. */
  public record Frame(byte[] payload, byte flags) {}

  /**
   * Read the next complete frame from the underlying stream. The returned future completes with the
   * frame on success, or completes exceptionally on a framing violation or stream termination
   * before a complete frame is available.
   */
  public CompletableFuture<Frame> readFrame() {
    return ensureAvailable(4)
        .thenCompose(
            v -> {
              int frameBodyLen =
                  ByteBuffer.wrap(buffer, 0, 4).order(ByteOrder.LITTLE_ENDIAN).getInt();
              if (frameBodyLen <= 0) {
                return CompletableFuture.failedFuture(
                    new FramingException("invalid frame length: " + frameBodyLen));
              }
              if (frameBodyLen > MAX_FRAME_SIZE) {
                return CompletableFuture.failedFuture(
                    new FramingException(
                        "frame size " + frameBodyLen + " exceeds max " + MAX_FRAME_SIZE));
              }
              int total = 4 + frameBodyLen;
              return ensureAvailable(total)
                  .thenApply(
                      v2 -> {
                        byte flags = buffer[4];
                        byte[] payload = new byte[frameBodyLen - 1];
                        System.arraycopy(buffer, 5, payload, 0, payload.length);
                        consume(total);
                        return new Frame(payload, flags);
                      });
            });
  }

  private CompletableFuture<Void> ensureAvailable(int n) {
    if (bufferLen >= n) {
      return CompletableFuture.completedFuture(null);
    }
    return chunkReader
        .apply(CHUNK_SIZE)
        .thenCompose(
            chunk -> {
              if (chunk == null || chunk.length == 0) {
                return CompletableFuture.failedFuture(
                    new FramingException(
                        "stream ended with " + bufferLen + " bytes buffered (needed " + n + ")"));
              }
              append(chunk);
              return ensureAvailable(n);
            });
  }

  private void append(byte[] chunk) {
    int needed = bufferLen + chunk.length;
    if (needed > buffer.length) {
      int newCap = Math.max(needed, Math.max(16, buffer.length * 2));
      byte[] grown = new byte[newCap];
      System.arraycopy(buffer, 0, grown, 0, bufferLen);
      buffer = grown;
    }
    System.arraycopy(chunk, 0, buffer, bufferLen, chunk.length);
    bufferLen += chunk.length;
  }

  private void consume(int n) {
    int remaining = bufferLen - n;
    if (remaining > 0) {
      System.arraycopy(buffer, n, buffer, 0, remaining);
    }
    bufferLen = remaining;
  }

  /** Raised when an incoming frame violates the wire format or the stream ends mid-frame. */
  public static final class FramingException extends RuntimeException {
    public FramingException(String message) {
      super(message);
    }
  }
}
