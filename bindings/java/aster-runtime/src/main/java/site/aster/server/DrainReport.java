package site.aster.server;

import java.time.Duration;

/**
 * What {@link AsterServer#drain(Duration)} did ({@code aster_drain_report_t}, ABI 7.2). Calls count
 * those admitted to a handler, not those refused at routing.
 *
 * @param callsInFlight calls running when the drain started
 * @param callsCompleted calls that finished before the deadline (those in flight at the start, and
 *     calls on existing sessions admitted during the drain)
 * @param callsCancelled calls still running at the deadline, cancelled by closing their connection
 * @param callsRefused new calls answered {@code UNAVAILABLE} with the {@code aster.draining} detail
 * @param connectionsClosed connections the drain closed with {@link #DRAIN_CLOSE_CODE}
 * @param connectionsRefused new connections refused at accept
 * @param deadlineReached the grace ended with work outstanding
 * @param elapsed how long the drain took
 */
public record DrainReport(
    long callsInFlight,
    long callsCompleted,
    long callsCancelled,
    long callsRefused,
    long connectionsClosed,
    long connectionsRefused,
    boolean deadlineReached,
    Duration elapsed) {

  /**
   * The QUIC application close code a draining server closes connections with ({@code
   * ASTER_DRAIN_CLOSE_CODE}): reconnect to another replica.
   */
  public static final long DRAIN_CLOSE_CODE = 503;

  /** The detail key a draining server sets on the {@code UNAVAILABLE} it answers new calls with. */
  public static final String DRAINING_DETAIL_KEY = "aster.draining";
}
