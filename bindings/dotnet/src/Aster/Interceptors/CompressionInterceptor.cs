namespace Aster.Interceptors;

/// <summary>
/// Per-call compression override interceptor.
/// Spec reference: S9.2g
///
/// Injects compression settings into the call metadata so downstream
/// transport code can honour them. Actual compression is performed by
/// the codec/framing layer.
/// </summary>
public class CompressionInterceptor : IInterceptor
{
    /// <summary>Default compression threshold in bytes.</summary>
    public const int DefaultCompressionThreshold = 4096;

    /// <summary>Payload size (bytes) above which compression is applied.</summary>
    public int Threshold { get; }

    /// <summary>Master switch. When false, compression is suppressed.</summary>
    public bool Enabled { get; }

    /// <param name="threshold">
    /// Payload size (bytes) above which compression is applied.
    /// Set to -1 to disable compression regardless of payload size.
    /// Defaults to 4096.
    /// </param>
    /// <param name="enabled">Master switch. When false, compression is suppressed.</param>
    public CompressionInterceptor(int threshold = DefaultCompressionThreshold, bool enabled = true)
    {
        Threshold = threshold;
        Enabled = enabled;
    }

    public object OnRequest(CallContext ctx, object request)
    {
        int effectiveThreshold = Enabled ? Threshold : -1;
        ctx.Metadata["_aster_compress_threshold"] = effectiveThreshold.ToString();
        ctx.Metadata["_aster_compress_enabled"] = Enabled.ToString().ToLowerInvariant();
        return request;
    }
}
