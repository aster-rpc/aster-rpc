namespace Aster.Interceptors;

/// <summary>
/// Simple CLOSED -> OPEN -> HALF_OPEN circuit breaker for client calls (same states and
/// thresholds as Python's <c>CircuitBreakerInterceptor</c>). The client call loop drives it
/// directly -- <see cref="BeforeCall"/> before every attempt, <see cref="RecordSuccess"/> /
/// <see cref="RecordFailure"/> after it -- so it has no request/response hooks. State
/// transitions are atomic (one breaker is shared by concurrent calls); one instance guards every
/// call made through the client or connection it is configured on, so use one instance per
/// peer / service you want to isolate.
/// </summary>
public class CircuitBreakerInterceptor : IInterceptor
{
    public const string StateClosed = "closed";
    public const string StateOpen = "open";
    public const string StateHalfOpen = "half_open";

    private static readonly HashSet<StatusCode> TrippingCodes = new()
    {
        StatusCode.Unavailable,
        StatusCode.Internal,
        StatusCode.Unknown,
    };

    private readonly object _gate = new();
    private string _state = StateClosed;
    private int _failureCount;
    private long _openedAtTicks;
    private int _halfOpenCalls;

    /// <summary>Number of consecutive failures before opening.</summary>
    public int FailureThreshold { get; }

    /// <summary>Seconds to wait before transitioning from Open to HalfOpen.</summary>
    public double RecoveryTimeout { get; }

    /// <summary>Max calls allowed in HalfOpen state before re-evaluating.</summary>
    public int HalfOpenMaxCalls { get; }

    /// <summary>Current circuit state (<see cref="StateClosed"/>, <see cref="StateOpen"/>, <see cref="StateHalfOpen"/>).</summary>
    public string State { get { lock (_gate) return _state; } }

    /// <summary>Consecutive failure count.</summary>
    public int FailureCount { get { lock (_gate) return _failureCount; } }

    public CircuitBreakerInterceptor(
        int failureThreshold = 3,
        double recoveryTimeout = 5.0,
        int halfOpenMaxCalls = 1)
    {
        FailureThreshold = failureThreshold;
        RecoveryTimeout = recoveryTimeout;
        HalfOpenMaxCalls = halfOpenMaxCalls;
    }

    /// <summary>
    /// Check circuit state before making a call. Throws <see cref="RpcError"/> (Unavailable) if
    /// the circuit is open, or half-open with its probe budget used.
    /// </summary>
    public void BeforeCall()
    {
        lock (_gate)
        {
            long now = Environment.TickCount64;

            if (_state == StateOpen)
            {
                double elapsed = (now - _openedAtTicks) / 1000.0;
                if (elapsed >= RecoveryTimeout)
                {
                    _state = StateHalfOpen;
                    _halfOpenCalls = 0;
                }
                else
                {
                    throw new RpcError(StatusCode.Unavailable, "circuit breaker is open");
                }
            }

            if (_state == StateHalfOpen)
            {
                if (_halfOpenCalls >= HalfOpenMaxCalls)
                {
                    throw new RpcError(StatusCode.Unavailable, "circuit breaker is half-open");
                }
                _halfOpenCalls++;
            }
        }
    }

    /// <summary>Record a successful call -- resets the circuit to closed.</summary>
    public void RecordSuccess()
    {
        lock (_gate)
        {
            _failureCount = 0;
            _halfOpenCalls = 0;
            _state = StateClosed;
        }
    }

    /// <summary>
    /// Record a failed call -- may trip the circuit to open. Only Unavailable, Internal and
    /// Unknown count. A half-open probe that fails with any other code frees its probe slot (the
    /// peer answered, so it neither closes nor re-opens the circuit) instead of leaving the
    /// breaker waiting for a result that will never be recorded.
    /// </summary>
    public void RecordFailure(RpcError error)
    {
        lock (_gate)
        {
            if (!TrippingCodes.Contains(error.Code))
            {
                if (_state == StateHalfOpen && _halfOpenCalls > 0)
                    _halfOpenCalls--;
                return;
            }

            if (_state == StateHalfOpen)
            {
                _state = StateOpen;
                _openedAtTicks = Environment.TickCount64;
                _halfOpenCalls = 0;
                return;
            }

            _failureCount++;
            if (_failureCount >= FailureThreshold)
            {
                _state = StateOpen;
                _openedAtTicks = Environment.TickCount64;
            }
        }
    }
}
