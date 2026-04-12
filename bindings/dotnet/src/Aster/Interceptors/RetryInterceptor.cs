namespace Aster.Interceptors;

/// <summary>
/// Provides retry policy hints for client calls.
/// Does not retry automatically -- exposes ShouldRetry and BackoffSeconds
/// for the call loop to consult.
/// </summary>
public class RetryInterceptor : IInterceptor
{
    /// <summary>Maximum number of attempts (including the first).</summary>
    public int MaxAttempts { get; }

    /// <summary>Status codes considered retryable.</summary>
    public HashSet<StatusCode> RetryableCodes { get; }

    // Backoff parameters
    private readonly int _initialMs;
    private readonly int _maxMs;
    private readonly double _multiplier;
    private readonly double _jitter;

    private static readonly Random Rng = new();

    /// <param name="maxAttempts">Maximum number of attempts (including the first). Defaults to 3.</param>
    /// <param name="retryableCodes">Status codes considered retryable. Defaults to {Unavailable}.</param>
    /// <param name="initialMs">Initial backoff delay in milliseconds. Defaults to 100.</param>
    /// <param name="maxMs">Maximum backoff delay in milliseconds. Defaults to 30000.</param>
    /// <param name="multiplier">Multiplicative factor per attempt. Defaults to 2.0.</param>
    /// <param name="jitter">Random jitter factor (0.0-1.0). Defaults to 0.1.</param>
    public RetryInterceptor(
        int maxAttempts = 3,
        HashSet<StatusCode>? retryableCodes = null,
        int initialMs = 100,
        int maxMs = 30_000,
        double multiplier = 2.0,
        double jitter = 0.1)
    {
        MaxAttempts = maxAttempts;
        RetryableCodes = retryableCodes ?? new HashSet<StatusCode> { StatusCode.Unavailable };
        _initialMs = initialMs;
        _maxMs = maxMs;
        _multiplier = multiplier;
        _jitter = jitter;
    }

    /// <summary>
    /// Whether the given error should be retried based on the call context.
    /// </summary>
    public bool ShouldRetry(CallContext ctx, RpcError error)
    {
        return ctx.Idempotent && RetryableCodes.Contains(error.Code);
    }

    /// <summary>
    /// Compute the backoff delay in seconds for the given attempt number.
    /// </summary>
    public double BackoffSeconds(int attempt)
    {
        int delayMs = Math.Min(
            _maxMs,
            (int)(_initialMs * Math.Pow(_multiplier, Math.Max(0, attempt - 1))));
        double jitterMs = delayMs * _jitter * Rng.NextDouble();
        return (delayMs + jitterMs) / 1000.0;
    }
}
