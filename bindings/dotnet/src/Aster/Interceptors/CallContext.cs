namespace Aster.Interceptors;

/// <summary>
/// Context for a single RPC call, available to interceptors and handlers.
/// </summary>
public class CallContext
{
    /// <summary>The service name (e.g., "MissionControl").</summary>
    public string Service { get; set; } = "";

    /// <summary>The method name (e.g., "GetStatus").</summary>
    public string Method { get; set; } = "";

    /// <summary>Unique ID for this call (auto-generated GUID).</summary>
    public string CallId { get; set; } = Guid.NewGuid().ToString();

    /// <summary>Session identifier, if applicable.</summary>
    public string? SessionId { get; set; }

    /// <summary>Remote peer identifier (endpoint ID hex).</summary>
    public string? Peer { get; set; }

    /// <summary>Key/value headers sent with the call.</summary>
    public Dictionary<string, string> Metadata { get; set; } = new();

    /// <summary>Enrollment attributes from the consumer's credential.</summary>
    public Dictionary<string, string> Attributes { get; set; } = new();

    /// <summary>Absolute deadline as epoch timestamp (seconds), or null.</summary>
    public double? Deadline { get; set; }

    /// <summary>True for streaming RPC patterns.</summary>
    public bool IsStreaming { get; set; }

    /// <summary>RPC pattern ("unary", "server_stream", etc.).</summary>
    public string? Pattern { get; set; }

    /// <summary>True if the method is safe to retry.</summary>
    public bool Idempotent { get; set; }

    /// <summary>Current retry attempt number (starts at 1).</summary>
    public int Attempt { get; set; } = 1;

    /// <summary>
    /// Returns the remaining seconds until deadline, or null if no deadline is set.
    /// </summary>
    public double? RemainingSeconds
    {
        get
        {
            if (Deadline is null)
                return null;
            var now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() / 1000.0;
            return Math.Max(0.0, Deadline.Value - now);
        }
    }

    /// <summary>
    /// True if the deadline has expired.
    /// </summary>
    public bool IsExpired
    {
        get
        {
            var remaining = RemainingSeconds;
            return remaining is not null && remaining <= 0.0;
        }
    }
}
