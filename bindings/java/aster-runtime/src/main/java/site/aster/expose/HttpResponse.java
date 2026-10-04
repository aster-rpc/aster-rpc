package site.aster.expose;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import site.aster.upcall.Ucv1.HttpHeader;

/**
 * A handler's answer: a status (100..=999), headers, and a body that is either inline (one round
 * trip: {@link #of}) or streamed after the head ({@link #streaming}). Immutable; {@link #header}
 * returns a copy.
 */
public final class HttpResponse {

  /** Writes a streamed body after the response head went out (see {@link ResponseBody}). */
  @FunctionalInterface
  public interface BodyWriter {
    void write(ResponseBody out) throws Exception;
  }

  private final int status;
  private final List<HttpHeader> headers;
  private final byte[] body;
  private final BodyWriter writer;

  private HttpResponse(int status, List<HttpHeader> headers, byte[] body, BodyWriter writer) {
    if (status < 100 || status > 999) {
      throw new IllegalArgumentException("HTTP status out of range: " + status);
    }
    this.status = status;
    this.headers = List.copyOf(headers);
    this.body = body;
    this.writer = writer;
  }

  /** An inline body, sent with the head. */
  public static HttpResponse of(int status, byte[] body) {
    return new HttpResponse(status, List.of(), body.clone(), null);
  }

  /** An inline UTF-8 text body with {@code content-type: text/plain; charset=utf-8}. */
  public static HttpResponse text(int status, String text) {
    return of(status, text.getBytes(StandardCharsets.UTF_8))
        .header("content-type", "text/plain; charset=utf-8");
  }

  /** A body written by {@code writer} through the exchange's body handle, with backpressure. */
  public static HttpResponse streaming(int status, BodyWriter writer) {
    if (writer == null) {
      throw new NullPointerException("writer");
    }
    return new HttpResponse(status, List.of(), null, writer);
  }

  /** A copy with header {@code name} (lowercased) added. */
  public HttpResponse header(String name, byte[] value) {
    List<HttpHeader> next = new ArrayList<>(headers);
    next.add(new HttpHeader(name.toLowerCase(Locale.ROOT), value.clone()));
    return new HttpResponse(status, next, body, writer);
  }

  public HttpResponse header(String name, String value) {
    return header(name, value.getBytes(StandardCharsets.UTF_8));
  }

  public int status() {
    return status;
  }

  public List<HttpHeader> headers() {
    return headers;
  }

  /** The inline body, or {@code null} for a streamed response. */
  public byte[] body() {
    return body == null ? null : body.clone();
  }

  public boolean isStreaming() {
    return writer != null;
  }

  BodyWriter writer() {
    return writer;
  }

  byte[] rawBody() {
    return body;
  }
}
