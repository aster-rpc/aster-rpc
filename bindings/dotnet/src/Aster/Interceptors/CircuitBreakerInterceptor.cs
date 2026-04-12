namespace Aster.Interceptors;

/// <summary>
/// Simple CLOSED -> OPEN -> HALF_OPEN circuit breaker.
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

    /// <summary>Number of consecutive failures before opening.</summary>
    public int FailureThreshold { get; }

    /// <summary>Seconds to wait before transitioning from Open to HalfOpen.</summary>
    public double RecoveryTimeout { get; }

    /// <summary>Max calls allowed in HalfOpen state before re-evaluating.</summary>
    public int HalfOpenMaxCalls { get; }

    /// <summary>Current circuit state.</summary>
    public string State { get; private set; } = StateClosed;

    /// <summary>Consecutive failure count.</summary>
    public int FailureCount { get; private set; }

    private long _openedAtTicks;
    private int _halfOpenCalls;

    public CircuitBreakerInterceptor(
        int failureThreshold = 3,
        double recoveryTimeout = 5.0,
        int halfOpenMaxCalls = 1)
    {
        FailureThreshold = failureThreshold;
        RecoveryTimeout = recoveryTimeout;
        HalfOpenMaxCalls = halfOpenMaxCalls;
    }

    public object OnRequest(CallContext ctx, object request)
    {
        BeforeCall();
        return request;
    }

    public RpcError? OnError(CallContext ctx, RpcError error)
    {
        RecordFailure(error);
        return error;
    }

    public object OnResponse(CallContext ctx, object response)
    {
        RecordSuccess();
        return response;
    }

    /// <summary>
    /// Check circuit state before making a call. Throws RpcError if circuit is open.
    /// </summary>
    public void BeforeCall()
    {
        long now = Environment.TickCount64;

        if (State == StateOpen)
        {
            double elapsed = (now - _openedAtTicks) / 1000.0;
            if (elapsed >= RecoveryTimeout)
            {
                State = StateHalfOpen;
                _halfOpenCalls = 0;
            }
            else
            {
                throw new RpcError(StatusCode.Unavailable, "circuit breaker is open");
            }
        }

        if (State == StateHalfOpen)
        {
            if (_halfOpenCalls >= HalfOpenMaxCalls)
            {
                throw new RpcError(StatusCode.Unavailable, "circuit breaker is half-open");
            }
            _halfOpenCalls++;
        }
    }

    /// <summary>Record a successful call -- resets the circuit to closed.</summary>
    public void RecordSuccess()
    {
        FailureCount = 0;
        _halfOpenCalls = 0;
        State = StateClosed;
    }

    /// <summary>Record a failed call -- may trip the circuit to open.</summary>
    public void RecordFailure(RpcError error)
    {
        if (!TrippingCodes.Contains(error.Code))
            return;

        if (State == StateHalfOpen)
        {
            State = StateOpen;
            _openedAtTicks = Environment.TickCount64;
            _halfOpenCalls = 0;
            return;
        }

        FailureCount++;
        if (FailureCount >= FailureThreshold)
        {
            State = StateOpen;
            _openedAtTicks = Environment.TickCount64;
        }
    }
}
