package site.aster.handle;

/**
 * One network path of a connection ({@code aster::PathInfo}).
 *
 * @param kind DIRECT, RELAY or OTHER
 * @param selected whether this path currently carries application data
 * @param peerHost DIRECT: the peer's IP address (else null)
 * @param peerPort DIRECT: the peer's UDP port (else 0)
 * @param relayUrl RELAY: the relay URL (else null)
 * @param other OTHER: a description of the transport (else null)
 * @param rttMicros RTT estimate in microseconds, or -1 if not yet measured
 */
public record PathInfo(
    Kind kind,
    boolean selected,
    String peerHost,
    int peerPort,
    String relayUrl,
    String other,
    long rttMicros) {

  /** {@code aster_path_kind_t}. */
  public enum Kind {
    NONE,
    DIRECT,
    RELAY,
    OTHER;

    static Kind fromCode(int code) {
      return code >= 0 && code < values().length ? values()[code] : OTHER;
    }
  }

  static PathInfo from(site.aster.ffi.DataPlane.RawPath p) {
    return new PathInfo(
        Kind.fromCode(p.kind()),
        p.selected(),
        p.peerHost(),
        p.peerPort(),
        p.relayUrl(),
        p.other(),
        p.rttMicros());
  }
}
