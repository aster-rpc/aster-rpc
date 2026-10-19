package site.aster.server;

/** Why a peer's last connection ended. */
public enum DisconnectReason {
  /** The peer, or this server, closed it normally. */
  CLOSED,
  /** It hit its idle timeout. */
  TIMED_OUT,
  /** The peer's credential expired or was revoked. */
  REVOKED,
  /** The server's drain closed it ({@link DrainReport#DRAIN_CLOSE_CODE}). */
  DRAINED,
  /** A transport error or reset. */
  ERROR
}
