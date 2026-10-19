//! The wait budget: how long a test waits for something that must happen.
//!
//! Not API. It lives here, hidden, because every crate's tests depend on this
//! one and must scale the same way. A test that waits for a reply, a delivered
//! cancel or a member becoming ready bounds the wait with [`wait_budget`]
//! rather than a literal: a loaded CI host (two jobs per Linux runner, one of
//! them compiling) took over 3 s to deliver a cancel that takes milliseconds
//! idle. `ASTER_TEST_TIME_SCALE` (default 1; CI sets it) multiplies the base.
//!
//! Never use it for a wait that is meant to expire ("nothing arrives within
//! X", a deadline under test): scaling those only slows the test down. Those
//! keep their literal with a `short-wait-ok: <reason>` comment
//! (`scripts/ci/lint_test_waits.py`).

use std::time::Duration;

/// The unscaled budget.
pub const WAIT_BUDGET_BASE: Duration = Duration::from_secs(15);

/// `ASTER_TEST_TIME_SCALE`, or 1 when unset or empty.
///
/// # Panics
///
/// When the variable is set to anything but a positive finite number: a
/// typo there would otherwise silently run CI unscaled.
pub fn time_scale() -> f64 {
    match std::env::var("ASTER_TEST_TIME_SCALE") {
        Ok(raw) if !raw.trim().is_empty() => match raw.trim().parse::<f64>() {
            Ok(scale) if scale.is_finite() && scale > 0.0 => scale,
            _ => panic!("ASTER_TEST_TIME_SCALE must be a positive number, got {raw:?}"),
        },
        _ => 1.0,
    }
}

/// How long to wait for something that must happen: 15 s times
/// [`time_scale`].
pub fn wait_budget() -> Duration {
    WAIT_BUDGET_BASE.mul_f64(time_scale())
}
