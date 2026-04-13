using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Aster.Registry;

/// <summary>
/// High-level .NET entry points into the Rust registry logic (§11).
/// </summary>
/// <remarks>
/// All resolution filtering and ranking is performed in Rust — this class is a thin wrapper
/// over the synchronous FFI functions. Doc reads and writes still go through the existing
/// node/docs FFI.
/// </remarks>
public static partial class Registry
{
    private const int InitialOutBuf = 16 * 1024;

    /// <summary>Single shared wall-clock reading used by freshness checks across languages.</summary>
    public static long NowEpochMs() => Native.aster_registry_now_epoch_ms();

    /// <summary>Return true if the given lease is still within the freshness window.</summary>
    public static unsafe bool IsFresh(EndpointLease lease, int leaseDurationS)
    {
        byte[] json = Encoding.UTF8.GetBytes(lease.ToJson());
        fixed (byte* p = json)
        {
            int result = Native.aster_registry_is_fresh(p, (UIntPtr)json.Length, leaseDurationS);
            if (result < 0)
                throw new InvalidOperationException($"aster_registry_is_fresh failed: {result}");
            return result == 1;
        }
    }

    /// <summary>Delegate to Rust to check whether a health string is READY or DEGRADED.</summary>
    public static unsafe bool IsRoutable(string status)
    {
        byte[] bytes = Encoding.UTF8.GetBytes(status);
        if (bytes.Length == 0)
            return false;
        fixed (byte* p = bytes)
        {
            return Native.aster_registry_is_routable(p, (UIntPtr)bytes.Length) == 1;
        }
    }

    /// <summary>
    /// Apply the §11.9 mandatory filters and ranking strategy to a list of leases via the Rust FFI.
    /// Returns the ranked survivors in best-first order; the top element is the resolved winner.
    /// </summary>
    public static unsafe List<EndpointLease> FilterAndRank(
        IReadOnlyList<EndpointLease> leases, ResolveOptions opts)
    {
        byte[] leasesJson = Encoding.UTF8.GetBytes(
            JsonSerializer.Serialize(leases, RegistryJsonContext.Default.IReadOnlyListEndpointLease));
        byte[] optsJson = Encoding.UTF8.GetBytes(opts.ToJson());

        int cap = InitialOutBuf;
        byte[] buf = new byte[cap];
        UIntPtr outLen = (UIntPtr)cap;

        fixed (byte* lp = leasesJson)
        fixed (byte* op = optsJson)
        fixed (byte* bp = buf)
        {
            int status = Native.aster_registry_filter_and_rank(
                lp, (UIntPtr)leasesJson.Length,
                op, (UIntPtr)optsJson.Length,
                bp, &outLen);

            if (status != 0)
            {
                long needed = (long)outLen;
                if (needed > cap)
                {
                    // Retry with a bigger buffer outside the fixed block.
                    return FilterAndRankWithBuffer(leasesJson, optsJson, (int)needed);
                }
                throw new InvalidOperationException($"aster_registry_filter_and_rank failed: {status}");
            }

            int written = (int)outLen;
            return JsonSerializer.Deserialize<List<EndpointLease>>(
                buf.AsSpan(0, written), RegistryJson.Options)
                ?? new List<EndpointLease>();
        }
    }

    private static unsafe List<EndpointLease> FilterAndRankWithBuffer(
        byte[] leasesJson, byte[] optsJson, int cap)
    {
        byte[] buf = new byte[cap];
        UIntPtr outLen = (UIntPtr)cap;
        fixed (byte* lp = leasesJson)
        fixed (byte* op = optsJson)
        fixed (byte* bp = buf)
        {
            int status = Native.aster_registry_filter_and_rank(
                lp, (UIntPtr)leasesJson.Length,
                op, (UIntPtr)optsJson.Length,
                bp, &outLen);
            if (status != 0)
                throw new InvalidOperationException($"aster_registry_filter_and_rank failed: {status}");
            int written = (int)outLen;
            return JsonSerializer.Deserialize<List<EndpointLease>>(
                buf.AsSpan(0, written), RegistryJson.Options)
                ?? new List<EndpointLease>();
        }
    }

    /// <summary>Delegate to Rust for a registry doc key (same bytes every binding sees).</summary>
    public static byte[] ContractKey(string contractId) => CallKey(0, contractId, "", "");
    public static byte[] VersionKey(string name, int version) => CallKey(1, name, version.ToString(), "");
    public static byte[] ChannelKey(string name, string channel) => CallKey(2, name, channel, "");
    public static byte[] LeaseKey(string name, string contractId, string endpointId) =>
        CallKey(3, name, contractId, endpointId);
    public static byte[] LeasePrefix(string name, string contractId) =>
        CallKey(4, name, contractId, "");
    public static byte[] AclKey(string subkey) => CallKey(5, subkey, "", "");

    private static unsafe byte[] CallKey(int kind, string a1, string a2, string a3)
    {
        byte[] b1 = Encoding.UTF8.GetBytes(a1);
        byte[] b2 = Encoding.UTF8.GetBytes(a2);
        byte[] b3 = Encoding.UTF8.GetBytes(a3);
        byte[] buf = new byte[512];
        UIntPtr outLen = (UIntPtr)buf.Length;
        fixed (byte* p1 = b1.Length > 0 ? b1 : new byte[1])
        fixed (byte* p2 = b2.Length > 0 ? b2 : new byte[1])
        fixed (byte* p3 = b3.Length > 0 ? b3 : new byte[1])
        fixed (byte* bp = buf)
        {
            int status = Native.aster_registry_key(
                kind,
                p1, (UIntPtr)b1.Length,
                p2, (UIntPtr)b2.Length,
                p3, (UIntPtr)b3.Length,
                bp, &outLen);
            if (status != 0)
                throw new InvalidOperationException($"aster_registry_key failed: {status}");
            int written = (int)outLen;
            byte[] result = new byte[written];
            Array.Copy(buf, result, written);
            return result;
        }
    }
}

/// <summary>Options controlling resolve filtering and ranking. Mirrors Rust ResolveOptions.</summary>
public sealed class ResolveOptions
{
    [JsonPropertyName("service")]
    public string Service { get; set; } = "";

    [JsonPropertyName("version")]
    public int? Version { get; set; }

    [JsonPropertyName("channel")]
    public string? Channel { get; set; }

    [JsonPropertyName("contract_id")]
    public string? ContractId { get; set; }

    [JsonPropertyName("strategy")]
    public string Strategy { get; set; } = "round_robin";

    [JsonPropertyName("caller_alpn")]
    public string CallerAlpn { get; set; } = "aster/1";

    [JsonPropertyName("caller_serialization_modes")]
    public List<string> CallerSerializationModes { get; set; } = new() { "fory-xlang" };

    [JsonPropertyName("caller_policy_realm")]
    public string? CallerPolicyRealm { get; set; }

    [JsonPropertyName("lease_duration_s")]
    public int LeaseDurationS { get; set; } = 45;

    public string ToJson() => JsonSerializer.Serialize(this, RegistryJson.Options);
}

[JsonSerializable(typeof(IReadOnlyList<EndpointLease>))]
[JsonSerializable(typeof(List<EndpointLease>))]
internal partial class RegistryJsonContext : JsonSerializerContext {}
