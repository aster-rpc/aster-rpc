using System;
using System.Globalization;

namespace Aster.Tests;

/// <summary>
/// How long a test waits for something that must happen. A test that waits for a reply, a
/// delivered cancel or a member becoming ready bounds the wait with <see cref="Value"/> rather
/// than a literal: a loaded CI host (two jobs per Linux runner, one of them compiling) took over
/// 3 s to deliver a deadline's server-side cancel that takes milliseconds idle.
/// <c>ASTER_TEST_TIME_SCALE</c> (default 1; CI sets it) multiplies the base.
/// <para>
/// Never use it for a wait that is meant to expire ("nothing arrives within X", a deadline under
/// test): scaling those only slows the test down. Those keep their literal with a
/// <c>short-wait-ok: &lt;reason&gt;</c> comment (scripts/ci/lint_test_waits.py).
/// </para>
/// </summary>
internal static class WaitBudget
{
    /// <summary>The unscaled budget.</summary>
    public static readonly TimeSpan Base = TimeSpan.FromSeconds(15);

    /// <summary><c>ASTER_TEST_TIME_SCALE</c>, or 1 when unset or empty.</summary>
    public static readonly double TimeScale = ReadScale();

    /// <summary>The budget: <see cref="Base"/> times <see cref="TimeScale"/>.</summary>
    public static readonly TimeSpan Value = Base * TimeScale;

    private static double ReadScale()
    {
        var raw = Environment.GetEnvironmentVariable("ASTER_TEST_TIME_SCALE")?.Trim();
        if (string.IsNullOrEmpty(raw)) return 1;
        // A typo here would otherwise silently run CI unscaled.
        if (!double.TryParse(raw, NumberStyles.Float, CultureInfo.InvariantCulture, out var scale)
            || !double.IsFinite(scale) || scale <= 0)
            throw new InvalidOperationException($"ASTER_TEST_TIME_SCALE must be a positive number, got '{raw}'");
        return scale;
    }
}
