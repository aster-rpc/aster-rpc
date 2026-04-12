namespace Aster.Interceptors;

/// <summary>
/// Captures structured audit events for requests, responses, and errors.
/// Events are appended to a sink (list of dictionaries) and optionally logged.
/// </summary>
public class AuditInterceptor : IInterceptor
{
    /// <summary>The audit event sink. Inspect after calls to review the trail.</summary>
    public List<Dictionary<string, object>> Sink { get; }

    /// <summary>Optional action for logging audit entries (e.g. Console.WriteLine).</summary>
    public Action<Dictionary<string, object>>? Logger { get; set; }

    /// <param name="sink">External sink list, or null to create a new one.</param>
    /// <param name="logger">Optional logging callback.</param>
    public AuditInterceptor(
        List<Dictionary<string, object>>? sink = null,
        Action<Dictionary<string, object>>? logger = null)
    {
        Sink = sink ?? new List<Dictionary<string, object>>();
        Logger = logger;
    }

    public object OnRequest(CallContext ctx, object request)
    {
        Record("request", ctx);
        return request;
    }

    public object OnResponse(CallContext ctx, object response)
    {
        Record("response", ctx);
        return response;
    }

    public RpcError? OnError(CallContext ctx, RpcError error)
    {
        Record("error", ctx,
            ("code", error.Code.ToString()),
            ("message", error.Message));
        return error;
    }

    private void Record(string eventName, CallContext ctx, params (string key, object value)[] extra)
    {
        var entry = new Dictionary<string, object>
        {
            ["event"] = eventName,
            ["service"] = ctx.Service,
            ["method"] = ctx.Method,
            ["call_id"] = ctx.CallId,
            ["attempt"] = ctx.Attempt,
            ["ts"] = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() / 1000.0,
        };
        foreach (var (key, value) in extra)
        {
            entry[key] = value;
        }
        Sink.Add(entry);
        Logger?.Invoke(entry);
    }
}
