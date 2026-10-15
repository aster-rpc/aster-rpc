using System.Text;

namespace Aster.Registry;

/// <summary>
/// Key-schema helpers for the Aster service registry.
/// All keys are UTF-8 encoded bytes suitable for iroh-docs set_bytes/query calls,
/// identical to <c>core/src/registry.rs</c>. See Aster-SPEC.md §11.2 and §12.4.
/// </summary>
/// <remarks>
/// The keys are prefix-free: no key is a byte prefix of another. iroh-docs lets a
/// write at a key (a value or a deletion marker) supersede the same author's older
/// entries whose keys extend it, so with <c>.../versions/v1</c> a prefix of
/// <c>.../versions/v10</c>, publishing or retracting v1 would remove v10. Every
/// segment ends in <c>/</c> and variable segments escape <c>%</c> and <c>/</c>
/// (<c>%25</c>, <c>%2F</c>), so a segment never holds a raw <c>/</c> and no family's
/// key is an interior path of another.
/// </remarks>
public static class RegistryKeys
{
    /// <summary>
    /// Registry download-policy prefixes: all key namespaces a registry client should sync.
    /// </summary>
    public static readonly byte[][] RegistryPrefixes =
    [
        Encoding.UTF8.GetBytes("contracts/"),
        Encoding.UTF8.GetBytes("services/"),
        Encoding.UTF8.GetBytes("endpoints/"),
        Encoding.UTF8.GetBytes("compatibility/"),
        Encoding.UTF8.GetBytes("_aster/"),
    ];

    /// <summary>Escape a variable key segment: <c>%</c> to <c>%25</c>, <c>/</c> to <c>%2F</c>.</summary>
    public static string KeySegment(string segment) =>
        segment.Replace("%", "%25").Replace("/", "%2F");

    public static byte[] ContractKey(string contractId) =>
        Encoding.UTF8.GetBytes($"contracts/{KeySegment(contractId)}/");

    public static byte[] ManifestKey(string contractId) =>
        Encoding.UTF8.GetBytes($"manifests/{KeySegment(contractId)}/");

    public static byte[] VersionKey(string name, int version) =>
        Encoding.UTF8.GetBytes($"services/{KeySegment(name)}/versions/v{version.ToString(System.Globalization.CultureInfo.InvariantCulture)}/");

    /// <summary>Every version pointer of one service, and nothing else.</summary>
    public static byte[] VersionPrefix(string name) =>
        Encoding.UTF8.GetBytes($"services/{KeySegment(name)}/versions/");

    public static byte[] ChannelKey(string name, string channel) =>
        Encoding.UTF8.GetBytes($"services/{KeySegment(name)}/channels/{KeySegment(channel)}/");

    public static byte[] TagKey(string name, string tag) =>
        Encoding.UTF8.GetBytes($"services/{KeySegment(name)}/tags/{KeySegment(tag)}/");

    public static byte[] LeaseKey(string name, string contractId, string endpointId) =>
        Encoding.UTF8.GetBytes(
            $"services/{KeySegment(name)}/contracts/{KeySegment(contractId)}/endpoints/{KeySegment(endpointId)}/");

    public static byte[] LeasePrefix(string name, string contractId) =>
        Encoding.UTF8.GetBytes($"services/{KeySegment(name)}/contracts/{KeySegment(contractId)}/endpoints/");

    public static byte[] AclKey(string subkey) =>
        Encoding.UTF8.GetBytes($"_aster/acl/{KeySegment(subkey)}/");

    public static byte[] ConfigKey(string subkey) =>
        Encoding.UTF8.GetBytes($"_aster/config/{KeySegment(subkey)}/");
}
