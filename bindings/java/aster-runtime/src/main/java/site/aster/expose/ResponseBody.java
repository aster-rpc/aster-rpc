package site.aster.expose;

import java.io.IOException;
import java.io.OutputStream;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;
import site.aster.upcall.Ucv1.HttpHeader;

/**
 * The streamed response body of an exchange, handed to a {@link HttpResponse.BodyWriter} after the
 * response head went out. Writes are copied into the exchange's bounded buffer (split at the
 * service's {@code maxChunk}) and wait while it is full, so a slow consumer slows the writer;
 * {@link HttpBodyException.Kind#CANCELLED} means the consumer is gone: stop producing. When the
 * writer returns, the body is finished (with {@link #trailers}); when it throws, the body is
 * aborted so the consumer sees a failed response, never a truncated one.
 */
public final class ResponseBody extends OutputStream {

  private final HttpBodies bodies;
  private final long streamId;
  private final int maxChunk;
  private final List<HttpHeader> trailers = new ArrayList<>();

  ResponseBody(HttpBodies bodies, long streamId, long maxChunk) {
    this.bodies = bodies;
    this.streamId = streamId;
    this.maxChunk = (int) Math.min(maxChunk, Integer.MAX_VALUE);
  }

  /** The exchange's {@code stream_id}. */
  public long streamId() {
    return streamId;
  }

  @Override
  public void write(int b) throws IOException {
    write(new byte[] {(byte) b}, 0, 1);
  }

  @Override
  public void write(byte[] b, int off, int len) throws IOException {
    java.util.Objects.checkFromIndexSize(off, len, b.length);
    int at = off;
    int end = off + len;
    while (at < end) {
      int n = Math.min(maxChunk, end - at);
      bodies.write(streamId, Arrays.copyOfRange(b, at, at + n), null);
      at += n;
    }
  }

  /** Trailers to send when the body finishes (a name is lowercased). */
  public void trailer(String name, byte[] value) {
    trailers.add(new HttpHeader(name.toLowerCase(Locale.ROOT), value.clone()));
  }

  List<HttpHeader> trailers() {
    return trailers;
  }
}
