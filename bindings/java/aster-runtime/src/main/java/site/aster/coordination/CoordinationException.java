package site.aster.coordination;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/**
 * A coordination failure, switched on {@link #kind()} (the {@code aster_coordination_error_t} of
 * the failing event), never on the message. {@link #status()} is the coarse {@code aster_status_t}:
 * {@code PERMISSION_DENIED} when the replicated ACL / admin predicate refused the signed request,
 * {@code UNAUTHENTICATED} when coordination admission did not authenticate the caller (see {@link
 * #isPermissionDenied()} / {@link #isUnauthenticated()}). A committed typed rejection is not an
 * exception: it is a resolved {@link Coordination.MutationReply}.
 */
public final class CoordinationException extends IrohException {

  private static final long serialVersionUID = 1L;

  /** {@code aster_coordination_error_t} plus the call-level failures of synchronous returns. */
  public enum Kind {
    /** A resolved result is unacknowledged: acknowledge it (or take it with unacknowledged()). */
    AWAITING_ACK(1),
    /** Another request is unresolved on this journal: resume() first. */
    UNRESOLVED(2),
    /** Another client holds this journal's lifetime lock (an open client, or closing ones). */
    JOURNAL_LOCKED(3),
    JOURNAL_FATAL(4),
    /** The group answered with an RPC status ({@link #rpcCode()}). */
    RPC(5),
    /** No member answered; the sequence stays open and resumes with the same bytes. */
    UNAVAILABLE(6),
    /** The client (or lease / manager) was closed. */
    CLOSED(7),
    REJECTED(8),
    INVALID(9),
    OTHER(10),
    /** Call-level: an unknown or already consumed handle. */
    NOT_FOUND(-2),
    /** Call-level: the operation needs a durable client (this one is read-only). */
    UNSUPPORTED(-3);

    public final int code;

    Kind(int code) {
      this.code = code;
    }

    static Kind of(int code) {
      for (Kind k : values()) {
        if (k.code == code) {
          return k;
        }
      }
      return OTHER;
    }
  }

  private final Kind kind;

  private CoordinationException(Kind kind, IrohException cause) {
    super(cause.status(), cause.errorCode(), cause.getMessage());
    initCause(cause);
    withEvent(cause.errorCode(), cause.eventFlags());
    this.kind = kind;
  }

  /** Translate a native failure (an error event, or a synchronous non-OK status). */
  static CoordinationException from(IrohException e) {
    if (e instanceof CoordinationException c) {
      return c;
    }
    Kind kind;
    if (e.errorCode() >= 1 && e.errorCode() <= 10) {
      kind = Kind.of(e.errorCode());
    } else {
      kind =
          switch (e.status()) {
            case ALREADY_CLOSED -> Kind.CLOSED;
            case INVALID_ARGUMENT -> Kind.INVALID;
            case NOT_FOUND -> Kind.NOT_FOUND;
            case UNSUPPORTED -> Kind.UNSUPPORTED;
            default -> Kind.OTHER;
          };
    }
    return new CoordinationException(kind, e);
  }

  public Kind kind() {
    return kind;
  }

  /** The RPC status code of a {@link Kind#RPC} failure, else 0. */
  public int rpcCode() {
    return kind == Kind.RPC ? eventFlags() : 0;
  }

  /** The replicated ACL / admin predicate refused the request (RPC 7). */
  public boolean isPermissionDenied() {
    return status() == IrohStatus.PERMISSION_DENIED;
  }

  /** Coordination admission did not authenticate the caller (RPC 16). */
  public boolean isUnauthenticated() {
    return status() == IrohStatus.UNAUTHENTICATED;
  }

  public boolean isJournalLocked() {
    return kind == Kind.JOURNAL_LOCKED;
  }
}
