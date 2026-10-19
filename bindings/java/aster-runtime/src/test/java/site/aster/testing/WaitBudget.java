package site.aster.testing;

import java.time.Duration;

/**
 * How long a test waits for something that must happen. A test that waits for a reply, a delivered
 * cancel or a member becoming ready bounds the wait with {@link #SECONDS} / {@link #duration()}
 * rather than a literal: a loaded CI host (two jobs per Linux runner, one of them compiling) took
 * over 3 s to deliver a cancel that takes milliseconds idle. {@code ASTER_TEST_TIME_SCALE} (default
 * 1; CI sets it) multiplies the base.
 *
 * <p>Never use it for a wait that is meant to expire ("nothing arrives within X", a deadline under
 * test): scaling those only slows the test down. Those keep their literal with a {@code
 * short-wait-ok: <reason>} comment (scripts/ci/lint_test_waits.py).
 */
public final class WaitBudget {
  /** The unscaled budget, in seconds. */
  public static final long BASE_SECONDS = 15;

  /** {@code ASTER_TEST_TIME_SCALE}, or 1 when unset or empty. */
  public static final double TIME_SCALE = readScale();

  /** The budget in whole seconds, for {@code get(WaitBudget.SECONDS, TimeUnit.SECONDS)}. */
  public static final long SECONDS = (long) Math.ceil(BASE_SECONDS * TIME_SCALE);

  private WaitBudget() {}

  /** The budget as a {@link Duration}. */
  public static Duration duration() {
    return Duration.ofSeconds(SECONDS);
  }

  private static double readScale() {
    String raw = System.getenv("ASTER_TEST_TIME_SCALE");
    if (raw == null || raw.isBlank()) {
      return 1;
    }
    double scale;
    try {
      scale = Double.parseDouble(raw.trim());
    } catch (NumberFormatException e) {
      scale = 0;
    }
    // A typo here would otherwise silently run CI unscaled.
    if (!Double.isFinite(scale) || scale <= 0) {
      throw new IllegalStateException(
          "ASTER_TEST_TIME_SCALE must be a positive number, got '" + raw + "'");
    }
    return scale;
  }
}
