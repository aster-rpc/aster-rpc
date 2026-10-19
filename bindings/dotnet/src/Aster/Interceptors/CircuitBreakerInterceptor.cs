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
    private readonly TimeProvider _clock;
    private string _state = StateClosed;
    private int _failureCount;
    private long _openedAt;
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

    /// <param name="failureThreshold">Consecutive failures before opening.</param>
    /// <param name="recoveryTimeout">Seconds to wait before transitioning from Open to HalfOpen.</param>
    /// <param name="halfOpenMaxCalls">Max calls allowed in HalfOpen state.</param>
    /// <param name="timeProvider">The clock the recovery timeout is measured on; <see cref="TimeProvider.System"/> by default (tests pass a fake one).</param>
    public CircuitBreakerInterceptor(
        int failureThreshold = 3,
        double recoveryTimeout = 5.0,
        int halfOpenMaxCalls = 1,
        TimeProvider? timeProvider = null)
    {
        FailureThreshold = failureThreshold;
        RecoveryTimeout = recoveryTimeout;
        HalfOpenMaxCalls = halfOpenMaxCalls;
        _clock = timeProvider ?? TimeProvider.System;
    }

    /// <summary>
    /// Check circuit state before making a call. Throws <see cref="RpcError"/> (Unavailable) if
    /// the circuit is open, or half-open with its probe budget used.
    /// </summary>
    public void BeforeCall()
    {
        lock (_gate)
        {
            if (_state == StateOpen)
            {
                double elapsed = _clock.GetElapsedTime(_openedAt).TotalSeconds;
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
    /// Give back a half-open probe slot taken by <see cref="BeforeCall"/> for a call whose outcome
    /// says nothing about the peer's health (refused by another breaker, or cancelled by the
    /// caller). Without it the breaker stays half-open with no free slot and refuses every later call.
    /// </summary>
    public void ReleaseProbe()
    {
        lock (_gate) ReleaseProbeLocked();
    }

    private void ReleaseProbeLocked()
    {
        if (_state == StateHalfOpen && _halfOpenCalls > 0)
            _halfOpenCalls--;
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
                ReleaseProbeLocked();
                return;
            }

            if (_state == StateHalfOpen)
            {
                _state = StateOpen;
                _openedAt = _clock.GetTimestamp();
                _halfOpenCalls = 0;
                return;
            }

            _failureCount++;
            if (_failureCount >= FailureThreshold)
            {
                _state = StateOpen;
                _openedAt = _clock.GetTimestamp();
            }
        }
    }
}
