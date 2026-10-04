package site.aster.expose;

import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Locale;
import site.aster.upcall.Ucv1.HttpHeader;

/** The whole response of {@link Expose#relayRequest}: any HTTP status, body fully buffered. */
public record RelayedResponse(int status, List<HttpHeader> headers, byte[] body) {

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

  public String bodyText() {
    return new String(body, StandardCharsets.UTF_8);
  }
}
