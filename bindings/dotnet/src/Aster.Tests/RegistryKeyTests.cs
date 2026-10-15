using System.Text;
using Aster.Registry;

namespace Aster.Tests;

/// <summary>
/// Registry keys: the documented format, prefix-free (iroh-docs lets a write at a
/// key supersede the same author's older entries whose keys extend it), and
/// byte-identical to core's builders reached through the C ABI.
/// </summary>
public class RegistryKeyTests
{
    private static string S(byte[] key) => Encoding.UTF8.GetString(key);

    [Fact]
    public void Format()
    {
        Assert.Equal("contracts/abc/", S(RegistryKeys.ContractKey("abc")));
        Assert.Equal("manifests/abc/", S(RegistryKeys.ManifestKey("abc")));
        Assert.Equal("services/svc/versions/v7/", S(RegistryKeys.VersionKey("svc", 7)));
        Assert.Equal("services/svc/versions/", S(RegistryKeys.VersionPrefix("svc")));
        Assert.Equal("services/svc/channels/stable/", S(RegistryKeys.ChannelKey("svc", "stable")));
        Assert.Equal("services/svc/tags/latest/", S(RegistryKeys.TagKey("svc", "latest")));
        Assert.Equal("services/svc/contracts/cid/endpoints/eid/", S(RegistryKeys.LeaseKey("svc", "cid", "eid")));
        Assert.Equal("services/svc/contracts/cid/endpoints/", S(RegistryKeys.LeasePrefix("svc", "cid")));
        Assert.Equal("_aster/acl/writers/", S(RegistryKeys.AclKey("writers")));
        Assert.Equal("_aster/config/version/", S(RegistryKeys.ConfigKey("version")));
        Assert.Equal("services/x/channels/a%2Fb%25c/", S(RegistryKeys.ChannelKey("x", "a/b%c")));
    }

    [Fact]
    public void PrefixFree()
    {
        string[] leaves = ["1", "10", "stable", "stable2", "x/y", "x", "", "%2F"];
        var keys = new List<string>();
        foreach (var name in new[] { "svc", "svc2", "a/b", "a", "50%" })
        {
            foreach (var leaf in leaves)
            {
                keys.Add(S(RegistryKeys.ContractKey(leaf)));
                keys.Add(S(RegistryKeys.ManifestKey(leaf)));
                keys.Add(S(RegistryKeys.AclKey(leaf)));
                keys.Add(S(RegistryKeys.ConfigKey(leaf)));
                keys.Add(S(RegistryKeys.ChannelKey(name, leaf)));
                keys.Add(S(RegistryKeys.TagKey(name, leaf)));
                foreach (var eid in leaves)
                    keys.Add(S(RegistryKeys.LeaseKey(name, leaf, eid)));
            }
            foreach (var v in new[] { 1, 10, 100, -1 })
                keys.Add(S(RegistryKeys.VersionKey(name, v)));
        }
        foreach (var a in keys)
            foreach (var b in keys)
                Assert.False(a != b && b.StartsWith(a, StringComparison.Ordinal), $"{a} is a prefix of {b}");
    }

    [Fact]
    public void MatchesRust()
    {
        Assert.Equal(Aster.Registry.Registry.ContractKey("abc"), RegistryKeys.ContractKey("abc"));
        Assert.Equal(Aster.Registry.Registry.VersionKey("a/b", 10), RegistryKeys.VersionKey("a/b", 10));
        Assert.Equal(Aster.Registry.Registry.ChannelKey("svc", "st/able"), RegistryKeys.ChannelKey("svc", "st/able"));
        Assert.Equal(Aster.Registry.Registry.LeaseKey("svc", "cid", "e%id"), RegistryKeys.LeaseKey("svc", "cid", "e%id"));
        Assert.Equal(Aster.Registry.Registry.LeasePrefix("svc", "cid"), RegistryKeys.LeasePrefix("svc", "cid"));
        Assert.Equal(Aster.Registry.Registry.AclKey("writers"), RegistryKeys.AclKey("writers"));
    }
}
