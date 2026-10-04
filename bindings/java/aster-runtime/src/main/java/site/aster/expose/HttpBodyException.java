package site.aster.expose;

import java.io.IOException;

/**
 * A body operation failed ({@code aster_http_body_status_t}). {@link #kind()} is the stable ABI
 * code; {@link Kind#CANCELLED} means the consumer went away and the handler should stop producing.
 */
public final class HttpBodyException extends IOException {

  private static final long serialVersionUID = 1L;

  /** {@code BodyError} codes 1-9 plus the two argument / handle errors. */
  public enum Kind {
    UNKNOWN_STREAM(1),
    /** The peer went away or the request was abandoned: stop producing. */
    CANCELLED(2),
    /** A body went over its cap (a read over {@code maxRequestBody}: answer 413). */
    TOO_LARGE(3),
    CHUNK_TOO_LARGE(4),
    FINISHED(5),
    TIMEOUT(6),
    /** Reading the request body off the wire failed. */
    READ(7),
    INVALID_TRAILERS(8),
    BLOCKING_ON_RUNTIME(9),
    /** Relay: the peer has no exposed service of this id. */
    UNKNOWN_SERVICE(10),
    /** Relay: the service's admission policy refused this node. */
    DENIED(11),
    /** Relay: the service is at its concurrent-stream cap; retry later. */
    OVERLOADED(12),
    /** Relay: the peer reset or ended the stream without a response head. */
    RESET(13),
    /** Relay: the response head was malformed or too large. */
    BAD_HEAD(14),
    /** Relay: the connection is closed or was lost. */
    CONNECTION_CLOSED(15),
    /** Relay: the upload was aborted (or freed unfinished) before the head. */
    UPLOAD_FAILED(16),
    BUFFER_TOO_SMALL(17),
    INVALID_ARGUMENT(-1),
    UNKNOWN_HANDLE(-2),
    /** A status this binding does not know (a newer library). */
    UNKNOWN(Integer.MIN_VALUE);

    public final int code;

    Kind(int code) {
      this.code = code;
    }

    static Kind fromCode(int code) {
      for (Kind k : values()) {
        if (k.code == code) {
          return k;
        }
      }
      return UNKNOWN;
    }
  }

  private final Kind kind;
  private final int code;

  HttpBodyException(int code, String message) {
    super(message);
    this.code = code;
    this.kind = Kind.fromCode(code);
  }

  HttpBodyException(Kind kind, String message) {
    super(message);
    this.code = kind.code;
    this.kind = kind;
  }

  public Kind kind() {
    return kind;
  }

  /** The stable ABI code. */
  public int code() {
    return code;
  }

  /** True when the exchange is gone (consumer left, or the exchange already ended): stop. */
  public boolean exchangeOver() {
    return kind == Kind.CANCELLED || kind == Kind.UNKNOWN_STREAM || kind == Kind.FINISHED;
  }
}
