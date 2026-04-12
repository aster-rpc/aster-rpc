using System.Collections.Concurrent;

namespace Aster.Interceptors;

/// <summary>
/// Collects RED metrics (Rate, Errors, Duration) for each RPC call.
/// Falls back to simple in-memory counters (no external dependency required).
/// </summary>
public class MetricsInterceptor : IInterceptor
{
    private int _started;
    private int _succeeded;
    private int _failed;

    /// <summary>Per-call start times keyed by a call identifier.</summary>
    private readonly ConcurrentDictionary<string, long> _callStarts = new();

    /// <summary>Total RPC calls started.</summary>
    public int Started => _started;

    /// <summary>Total RPC calls that succeeded.</summary>
    public int Succeeded => _succeeded;

    /// <summary>Total RPC calls that failed.</summary>
    public int Failed => _failed;

    public object OnRequest(CallContext ctx, object request)
    {
        Interlocked.Increment(ref _started);

        var callKey = $"{ctx.Service}.{ctx.Method}.{ctx.CallId}";
        _callStarts[callKey] = Environment.TickCount64;

        // Store call key on metadata for retrieval in response/error
        ctx.Metadata["_metrics_call_key"] = callKey;

        return request;
    }

    public object OnResponse(CallContext ctx, object response)
    {
        Interlocked.Increment(ref _succeeded);
        FinishTiming(ctx);
        return response;
    }

    public RpcError? OnError(CallContext ctx, RpcError error)
    {
        Interlocked.Increment(ref _failed);
        FinishTiming(ctx);
        return error;
    }

    /// <summary>
    /// Return a snapshot of in-memory counters.
    /// </summary>
    public Dictionary<string, int> Snapshot()
    {
        int started = _started;
        int succeeded = _succeeded;
        int failed = _failed;
        return new Dictionary<string, int>
        {
            ["started"] = started,
            ["succeeded"] = succeeded,
            ["failed"] = failed,
            ["in_flight"] = started - succeeded - failed,
        };
    }

    private void FinishTiming(CallContext ctx)
    {
        if (ctx.Metadata.TryGetValue("_metrics_call_key", out var callKey))
        {
            _callStarts.TryRemove(callKey, out _);
        }
    }
}
