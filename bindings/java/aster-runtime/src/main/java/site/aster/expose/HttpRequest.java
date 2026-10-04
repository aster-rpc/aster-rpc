package site.aster.expose;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Locale;
import site.aster.upcall.Ucv1.HttpHeader;

/**
 * One request for an {@link HttpHandler}. {@code peerId} is the transport-authenticated node
 * ({@code null} off the relay: refuse it if you attribute requests). {@code body} is a stream over
 * the request body: in memory when the request declared a {@code content-length} of at most the
 * service's {@code inlineRequestMax}, otherwise pulled from the QUIC stream as you read (Rust reads
 * only while you do). Read the request body before the response ends: when the relay is done with
 * the exchange, further reads fail with {@link HttpBodyException.Kind#UNKNOWN_STREAM}.
 */
public record HttpRequest(
    String serviceId,
    String peerId,
    long streamId,
    HttpVersion version,
    String method,
    String uri,
    List<HttpHeader> headers,
    InputStream body,
    boolean inline) {

  /** The first value of header {@code name} (case-insensitive), or {@code null}. */
  public byte[] header(String name) {
    String n = name.toLowerCase(Locale.ROOT);
    for (HttpHeader h : headers) {
      if (h.name().equals(n)) {
        return h.value();
      }
    }
    return null;
  }

  /** {@link #header} as UTF-8 text, or {@code null}. */
  public String headerText(String name) {
    byte[] v = header(name);
    return v == null ? null : new String(v, StandardCharsets.UTF_8);
  }

  /**
   * The whole request body. {@code limit} (bytes, {@code < 0} = none): fails with {@link
   * HttpBodyException.Kind#TOO_LARGE} once the body grows past it.
   */
  public byte[] readBody(long limit) throws IOException {
    java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
    byte[] buf = new byte[8192];
    long total = 0;
    int n;
    while ((n = body.read(buf)) >= 0) {
      total += n;
      if (limit >= 0 && total > limit) {
        throw new HttpBodyException(
            HttpBodyException.Kind.TOO_LARGE, "request body exceeds " + limit + " bytes");
      }
      out.write(buf, 0, n);
    }
    return out.toByteArray();
  }

  public byte[] readBody() throws IOException {
    return readBody(-1);
  }
}
