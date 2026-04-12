namespace Aster.Interceptors;

/// <summary>
/// Base interceptor interface. All methods have default pass-through behavior.
/// Implementors override only the hooks they need.
/// </summary>
public interface IInterceptor
{
    /// <summary>Intercept an outgoing/incoming request. Return the (possibly modified) request.</summary>
    object OnRequest(CallContext ctx, object request) => request;

    /// <summary>Intercept a response before it reaches the caller. Return the (possibly modified) response.</summary>
    object OnResponse(CallContext ctx, object response) => response;

    /// <summary>Intercept an error. Return the error, a replacement, or null to suppress it.</summary>
    RpcError? OnError(CallContext ctx, RpcError error) => error;
}
