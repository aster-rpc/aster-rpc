namespace Aster.Interceptors;

/// <summary>
/// Applies a list of interceptors in order (request/response forward, error reverse).
/// </summary>
public static class InterceptorChain
{
    /// <summary>
    /// Run each interceptor's OnRequest in forward order.
    /// </summary>
    public static object ApplyRequest(IReadOnlyList<IInterceptor> interceptors, CallContext ctx, object request)
    {
        var current = request;
        foreach (var interceptor in interceptors)
        {
            current = interceptor.OnRequest(ctx, current);
        }
        return current;
    }

    /// <summary>
    /// Run each interceptor's OnResponse in forward order.
    /// </summary>
    public static object ApplyResponse(IReadOnlyList<IInterceptor> interceptors, CallContext ctx, object response)
    {
        var current = response;
        foreach (var interceptor in interceptors)
        {
            current = interceptor.OnResponse(ctx, current);
        }
        return current;
    }

    /// <summary>
    /// Run each interceptor's OnError in reverse order. Returns null if any interceptor suppresses the error.
    /// </summary>
    public static RpcError? ApplyError(IReadOnlyList<IInterceptor> interceptors, CallContext ctx, RpcError error)
    {
        RpcError? current = error;
        for (int i = interceptors.Count - 1; i >= 0; i--)
        {
            if (current is null)
                return null;
            current = interceptors[i].OnError(ctx, current);
        }
        return current;
    }
}
