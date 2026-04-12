namespace Aster.Interceptors;

/// <summary>
/// Validates and enforces call deadlines with configurable clock-skew tolerance.
/// Spec reference: S6.8.1
/// </summary>
public class DeadlineInterceptor : IInterceptor
{
    private readonly int _skewToleranceMs;

    /// <param name="skewToleranceMs">
    /// Milliseconds of clock-skew tolerance. A request whose deadline has
    /// already passed by more than this tolerance is rejected immediately.
    /// Defaults to 5000 ms (5 seconds).
    /// </param>
    public DeadlineInterceptor(int skewToleranceMs = 5000)
    {
        _skewToleranceMs = skewToleranceMs;
    }

    public object OnRequest(CallContext ctx, object request)
    {
        if (ctx.Deadline is not null)
        {
            long nowEpochMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
            long deadlineEpochMs = (long)(ctx.Deadline.Value * 1000);

            // Reject on receipt if expired beyond skew tolerance (S6.8.1)
            if (nowEpochMs > deadlineEpochMs + _skewToleranceMs)
            {
                throw new RpcError(
                    StatusCode.DeadlineExceeded,
                    $"deadline already expired on receipt (now={nowEpochMs}, deadline={deadlineEpochMs}, skew_tolerance={_skewToleranceMs}ms)");
            }

            // Standard expiry check (no tolerance)
            if (ctx.IsExpired)
            {
                throw new RpcError(StatusCode.DeadlineExceeded, "deadline exceeded");
            }
        }
        return request;
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
