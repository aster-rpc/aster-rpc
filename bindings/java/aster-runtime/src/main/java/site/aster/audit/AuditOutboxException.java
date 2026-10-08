package site.aster.audit;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/** Durable audit failure classified by the native error code, never exception text. */
public final class AuditOutboxException extends IrohException {
  private static final long serialVersionUID = 1L;

  public enum Kind {
    INVALID_CONFIG(1),
    INVALID_EVENT(2),
    EVENT_TOO_LARGE(3),
    STORAGE_FULL(4),
    STORAGE(5),
    COMMIT_UNCERTAIN(6),
    QUEUE_OVERLOADED(7),
    MAINTENANCE(8),
    CLOSED(9),
    WORKER_FAILED(10),
    TIMEOUT(11),
    DELIVERY_STALLED(12),
    DISPOSITION(13),
    NOT_FOUND(14),
    DESTINATION_MISMATCH(15),
    INTEGRITY_CONFLICT(16),
    STALE_DELIVERY(17),
    OTHER(0);

    public final int code;

    Kind(int code) {
      this.code = code;
    }

    static Kind of(int code) {
      for (Kind kind : values()) if (kind.code == code) return kind;
      return OTHER;
    }
  }

  private final Kind kind;

  public AuditOutboxException(Kind kind, String message) {
    super(IrohStatus.INTERNAL, kind.code, message);
    this.kind = kind;
  }

  private AuditOutboxException(Kind kind, IrohException cause) {
    super(cause.status(), cause.errorCode(), cause.getMessage());
    initCause(cause);
    withEvent(cause.errorCode(), cause.eventFlags());
    this.kind = kind;
  }

  static AuditOutboxException from(IrohException cause) {
    if (cause instanceof AuditOutboxException audit) return audit;
    Kind kind = Kind.of(cause.errorCode());
    if (kind == Kind.OTHER) {
      kind =
          switch (cause.status()) {
            case ALREADY_CLOSED -> Kind.CLOSED;
            case NOT_FOUND -> Kind.NOT_FOUND;
            case INVALID_ARGUMENT -> Kind.INVALID_CONFIG;
            default -> Kind.OTHER;
          };
    }
    return new AuditOutboxException(kind, cause);
  }

  public Kind kind() {
    return kind;
  }
}
