package site.aster.expose;

/**
 * A route an edge binds for a peer: {@code host} ({@code ""} = any) and {@code port} ({@code 0} =
 * the listener's default) to match, carried as {@code protocol} to the origin's exposed {@code
 * serviceId}.
 */
public record RouteSpec(String host, int port, Protocol protocol, String serviceId) {

  /** {@code aster_route_protocol_t}. */
  public enum Protocol {
    HTTP(0),
    TCP(1);

    public final int code;

    Protocol(int code) {
      this.code = code;
    }

    static Protocol fromCode(int code) {
      return code == 1 ? TCP : HTTP;
    }
  }

  public RouteSpec {
    if (host == null || serviceId == null || protocol == null) {
      throw new NullPointerException("host, protocol and serviceId are required");
    }
    if (port < 0 || port > 0xFFFF) {
      throw new IllegalArgumentException("port out of range: " + port);
    }
  }

  /** An HTTP route for {@code host} to {@code serviceId} on the listener's default port. */
  public static RouteSpec http(String host, String serviceId) {
    return new RouteSpec(host, 0, Protocol.HTTP, serviceId);
  }
}
