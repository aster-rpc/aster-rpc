namespace Aster.Interceptors;

/// <summary>
/// Validates and enforces call deadlines with configurable clock-skew tolerance.
/// Spec reference: S6.8.1. On the client it bounds every attempt of a call to the time left of
/// the original deadline (the remaining time is what the native call is given, so retries do not
/// extend it) and refuses to send a call whose deadline has passed.
/// </summary>
public class DeadlineInterceptor : IInterceptor
{
    private readonly int _skewToleranceMs;

    /// <summary>The clock deadlines are read against.</summary>
    internal TimeProvider Clock { get; }

    /// <param name="skewToleranceMs">
    /// Milliseconds of clock-skew tolerance. A request whose deadline has
    /// already passed by more than this tolerance is rejected immediately.
    /// Defaults to 5000 ms (5 seconds).
    /// </param>
    /// <param name="timeProvider">The clock deadlines are read against; <see cref="TimeProvider.System"/> by default (tests pass a fake one).</param>
    public DeadlineInterceptor(int skewToleranceMs = 5000, TimeProvider? timeProvider = null)
    {
        _skewToleranceMs = skewToleranceMs;
        Clock = timeProvider ?? TimeProvider.System;
    }

    public object OnRequest(CallContext ctx, object request)
    {
        CheckDeadline(ctx);
        return request;
    }

    /// <summary>
    /// Throws <see cref="StatusCode.DeadlineExceeded"/> if the call's deadline has passed (beyond
    /// the skew tolerance on receipt, or at all). The client runs it before every attempt, also
    /// for client-streaming and bidi calls that have no single request to hook.
    /// </summary>
    public void CheckDeadline(CallContext ctx)
    {
        if (ctx.Deadline is null) return;
        long nowEpochMs = Clock.GetUtcNow().ToUnixTimeMilliseconds();
        long deadlineEpochMs = (long)(ctx.Deadline.Value * 1000);

        // Reject on receipt if expired beyond skew tolerance (S6.8.1)
        if (nowEpochMs > deadlineEpochMs + _skewToleranceMs)
        {
            throw new RpcError(
                StatusCode.DeadlineExceeded,
                $"deadline already expired on receipt (now={nowEpochMs}, deadline={deadlineEpochMs}, skew_tolerance={_skewToleranceMs}ms)");
        }

        // Standard expiry check (no tolerance)
        if (nowEpochMs >= deadlineEpochMs)
        {
            throw new RpcError(StatusCode.DeadlineExceeded, "deadline exceeded");
        }
    }

    /// <summary>
    /// Return remaining seconds until deadline, or null if no deadline set.
    /// </summary>
    public double? TimeoutSeconds(CallContext ctx)
    {
        var remaining = ctx.RemainingSeconds;
        if (remaining is null)
            return null;
        return Math.Max(0.0, remaining.Value);
    }
}
