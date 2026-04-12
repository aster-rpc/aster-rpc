namespace Aster.Interceptors;

/// <summary>
/// Enforces capability requirements on incoming RPC calls.
///
/// Checks service-level and method-level role requirements against the
/// caller's admission attributes (CallContext.Attributes).
/// </summary>
public class CapabilityInterceptor : IInterceptor
{
    private readonly Dictionary<string, ServiceRequirements> _serviceMap;

    /// <param name="serviceMap">
    /// Mapping of service name to its role requirements.
    /// </param>
    public CapabilityInterceptor(Dictionary<string, ServiceRequirements> serviceMap)
    {
        _serviceMap = serviceMap;
    }

    public object OnRequest(CallContext ctx, object request)
    {
        if (!_serviceMap.TryGetValue(ctx.Service, out var svcReqs))
            return request;

        // Service-level requirement
        if (svcReqs.RequiredRole is not null)
        {
            if (!HasRole(ctx.Attributes, svcReqs.RequiredRole))
            {
                throw new RpcError(
                    StatusCode.PermissionDenied,
                    $"capability check failed for service '{ctx.Service}'");
            }
        }

        // Method-level requirement
        if (svcReqs.MethodRoles.TryGetValue(ctx.Method, out var methodRole))
        {
            if (!HasRole(ctx.Attributes, methodRole))
            {
                throw new RpcError(
                    StatusCode.PermissionDenied,
                    $"capability check failed for method '{ctx.Service}.{ctx.Method}'");
            }
        }

        return request;
    }

    private static bool HasRole(Dictionary<string, string> attributes, string requiredRole)
    {
        // Check if the "role" attribute matches or contains the required role
        if (!attributes.TryGetValue("role", out var roleValue))
            return false;

        // Support comma-separated roles
        var roles = roleValue.Split(',', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries);
        foreach (var role in roles)
        {
            if (string.Equals(role, requiredRole, StringComparison.OrdinalIgnoreCase))
                return true;
        }
        return false;
    }
}

/// <summary>
/// Describes the role requirements for a service and its methods.
/// </summary>
public class ServiceRequirements
{
    /// <summary>Service-level required role, or null if none.</summary>
    public string? RequiredRole { get; set; }

    /// <summary>Method name to required role mapping.</summary>
    public Dictionary<string, string> MethodRoles { get; set; } = new();
}
