/**
 * The wait budget: how long a test waits for something that must happen.
 *
 * A test that waits for a reply, a delivered cancel or a member becoming
 * ready bounds the wait with `WAIT_BUDGET_MS` rather than a literal: a loaded
 * CI host (two jobs per Linux runner, one of them compiling) took over 3 s to
 * deliver a cancel that takes milliseconds idle. `ASTER_TEST_TIME_SCALE`
 * (default 1; CI sets it) multiplies the base, and vitest.config.ts scales
 * the per-test timeout by the same factor.
 *
 * Never use it for a wait that is meant to expire ("nothing arrives within
 * X", a deadline under test): scaling those only slows the test down. Those
 * keep their literal with a `short-wait-ok: <reason>` comment
 * (scripts/ci/lint_test_waits.py).
 */

export const WAIT_BUDGET_BASE_MS = 15_000;

/** `ASTER_TEST_TIME_SCALE`, or 1 when unset or empty. */
export function timeScale(): number {
  const raw = (process.env.ASTER_TEST_TIME_SCALE ?? '').trim();
  if (!raw) return 1;
  const scale = Number(raw);
  // A typo here would otherwise silently run CI unscaled.
  if (!Number.isFinite(scale) || scale <= 0) {
    throw new Error(`ASTER_TEST_TIME_SCALE must be a positive number, got ${JSON.stringify(raw)}`);
  }
  return scale;
}

export const TIME_SCALE = timeScale();
export const WAIT_BUDGET_MS = WAIT_BUDGET_BASE_MS * TIME_SCALE;
