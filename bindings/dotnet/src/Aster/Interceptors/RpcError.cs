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

    /// <summary>The <see cref="Details"/> key (value <c>"true"</c>) a draining server sets on its <c>UNAVAILABLE</c> answer.</summary>
    public const string DrainingDetailKey = "aster.draining";

    /// <summary>
    /// The server was draining (<see cref="AsterServer.DrainAsync"/>): it refused the call, or closed the
    /// connection under it. Retrying on the same connection cannot succeed; connect to another replica.
    /// </summary>
    public bool IsDraining => Code == StatusCode.Unavailable && Details.ContainsKey(DrainingDetailKey);

    /// <summary>Whether <paramref name="error"/> is (or wraps) a draining server's answer (see <see cref="IsDraining"/>).</summary>
    public static bool IsDrainingError(Exception? error)
    {
        for (var e = error; e is not null; e = e.InnerException)
            if (e is RpcError { IsDraining: true }) return true;
        return false;
    }

    public override string ToString()
        => $"RpcError(Code={Code}, Message=\"{Message}\", Details=[{Details.Count} entries])";
}
