using System.Text;

namespace Aster.Registry;

/// <summary>
/// Key-schema helpers for the Aster service registry.
/// All keys are UTF-8 encoded bytes suitable for iroh-docs set_bytes/query calls.
/// See Aster-SPEC.md §11.2 and §12.4 for the normative prefixes.
/// </summary>
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

    public static byte[] ContractKey(string contractId) =>
        Encoding.UTF8.GetBytes($"contracts/{contractId}");

    public static byte[] VersionKey(string name, int version) =>
        Encoding.UTF8.GetBytes($"services/{name}/versions/v{version}");

    public static byte[] ChannelKey(string name, string channel) =>
        Encoding.UTF8.GetBytes($"services/{name}/channels/{channel}");

    public static byte[] TagKey(string name, string tag) =>
        Encoding.UTF8.GetBytes($"services/{name}/tags/{tag}");

    public static byte[] LeaseKey(string name, string contractId, string endpointId) =>
        Encoding.UTF8.GetBytes($"services/{name}/contracts/{contractId}/endpoints/{endpointId}");

    public static byte[] LeasePrefix(string name, string contractId) =>
        Encoding.UTF8.GetBytes($"services/{name}/contracts/{contractId}/endpoints/");

    public static byte[] AclKey(string subkey) =>
        Encoding.UTF8.GetBytes($"_aster/acl/{subkey}");

    public static byte[] ConfigKey(string subkey) =>
        Encoding.UTF8.GetBytes($"_aster/config/{subkey}");
}
