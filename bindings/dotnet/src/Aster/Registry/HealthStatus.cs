namespace Aster.Registry;

/// <summary>
/// Health state of a registered endpoint (Aster-SPEC.md §11.6).
/// </summary>
/// <remarks>
/// Modeled as a constants class (not enum) because the wire format uses lowercase
/// string values that pass through the registry doc and FFI layer untouched.
/// </remarks>
public static class HealthStatus
{
    public const string Starting = "starting";
    public const string Ready = "ready";
    public const string Degraded = "degraded";
    public const string Draining = "draining";

    public static bool IsValid(string value) =>
        value is Starting or Ready or Degraded or Draining;

    public static string Validate(string value)
    {
        if (!IsValid(value))
            throw new ArgumentException($"Invalid HealthStatus: {value}", nameof(value));
        return value;
    }

    public static bool IsRoutable(string status) =>
        status is Ready or Degraded;
}
