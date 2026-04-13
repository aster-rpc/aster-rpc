package site.aster.ffi;

/**
 * Thrown by {@link AsterCall#acquire(long, long, int)} when the per-connection multiplexed stream
 * pool cannot hand out a stream. Subcode identifies the reason so callers can react: raise pool
 * size, retry later, abandon the connection, etc. (spec §5, §8).
 */
public final class StreamAcquireException extends RuntimeException {

  public enum Reason {
    POOL_FULL,
    QUIC_LIMIT_REACHED,
    PEER_STREAM_LIMIT_TOO_LOW,
    STREAM_OPEN_FAILED,
    POOL_CLOSED
  }

  private final Reason reason;

  public StreamAcquireException(Reason reason, String message) {
    super(message);
    this.reason = reason;
  }

  public Reason reason() {
    return reason;
  }
}
