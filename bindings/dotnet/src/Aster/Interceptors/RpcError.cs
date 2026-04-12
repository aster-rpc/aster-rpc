namespace Aster.Interceptors;

/// <summary>
/// Exception raised when an RPC call fails.
/// </summary>
public class RpcError : Exception
{
    /// <summary>The status code describing the failure category.</summary>
    public StatusCode Code { get; }

    /// <summary>A human-readable error description.</summary>
    public new string Message { get; }

    /// <summary>Arbitrary key/value pairs carrying extra context.</summary>
    public Dictionary<string, string> Details { get; }

    public RpcError(StatusCode code, string message = "", Dictionary<string, string>? details = null)
        : base($"[{code}] {message}")
    {
        Code = code;
        Message = message;
        Details = details ?? new Dictionary<string, string>();
    }

    public override string ToString()
        => $"RpcError(Code={Code}, Message=\"{Message}\", Details=[{Details.Count} entries])";
}
