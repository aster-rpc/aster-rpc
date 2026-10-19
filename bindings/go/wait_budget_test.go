package aster

import (
	"fmt"
	"math"
	"os"
	"strconv"
	"strings"
	"time"
)

// waitBudgetBase is the unscaled wait budget.
const waitBudgetBase = 15 * time.Second

// waitBudget is how long a test waits for something that must happen. A test
// that waits for a reply, a delivered cancel or a member becoming ready bounds
// the wait with it rather than a literal: a loaded CI host (two jobs per Linux
// runner, one of them compiling) took over 3 s to deliver a cancel that takes
// milliseconds idle. ASTER_TEST_TIME_SCALE (default 1; CI sets it) multiplies
// the base.
//
// Never use it for a wait that is meant to expire ("nothing arrives within X",
// a deadline under test): scaling those only slows the test down. Those keep
// their literal with a `short-wait-ok: <reason>` comment
// (scripts/ci/lint_test_waits.py).
func waitBudget() time.Duration {
	return time.Duration(float64(waitBudgetBase) * testTimeScale())
}

// testTimeScale is ASTER_TEST_TIME_SCALE, or 1 when unset or empty. A value
// that is not a positive number panics: a typo there would otherwise silently
// run CI unscaled.
func testTimeScale() float64 {
	raw := strings.TrimSpace(os.Getenv("ASTER_TEST_TIME_SCALE"))
	if raw == "" {
		return 1
	}
	scale, err := strconv.ParseFloat(raw, 64)
	if err != nil || scale <= 0 || math.IsInf(scale, 0) || math.IsNaN(scale) {
		panic(fmt.Sprintf("ASTER_TEST_TIME_SCALE must be a positive number, got %q", raw))
	}
	return scale
}
