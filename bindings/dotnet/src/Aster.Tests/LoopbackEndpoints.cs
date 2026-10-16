using System;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Tests;

/// <summary>
/// Endpoints that never leave the machine: relay disabled, bound to 127.0.0.1:0, no address lookup
/// (no n0 pkarr/DNS, no mDNS), dialled by full address once the peer reports a direct address. With
/// the default relay a node start blocks until the public n0 relay answers and a connect by bare id
/// resolves the peer through n0 DNS/pkarr, so a test built that way depends on the internet.
/// </summary>
internal static class LoopbackEndpoints
{
    /// <summary>How long a peer may take to report a direct address.</summary>
    private static readonly TimeSpan AddressBudget = TimeSpan.FromSeconds(10);

    /// <summary>A loopback-only endpoint configuration accepting <paramref name="alpn"/>.</summary>
    public static EndpointConfigBuilder Config(string alpn) => new EndpointConfigBuilder()
        .Alpn(alpn)
        .SetRelayMode(RelayMode.Disabled)
        .BindAddr("127.0.0.1:0")
        .EnableDiscovery(false);

    /// <summary>Starts a loopback-only endpoint on <paramref name="runtime"/>.</summary>
    public static Task<Endpoint> CreateAsync(Runtime runtime, string alpn, CancellationToken ct = default)
        => Endpoint.CreateAsync(runtime, Config(alpn), ct);

    /// <summary><paramref name="ep"/>'s address once it reports a direct address; fails the test otherwise.</summary>
    public static async Task<NodeAddrInfo> AddrAsync(Endpoint ep, CancellationToken ct = default)
    {
        var deadline = DateTime.UtcNow + AddressBudget;
        var addr = ep.NodeAddr();
        while (addr.DirectAddresses.Count == 0 && DateTime.UtcNow < deadline)
        {
            await Task.Delay(20, ct);
            addr = ep.NodeAddr();
        }
        Assert.True(addr.DirectAddresses.Count > 0, $"endpoint {addr.EndpointId} reported no direct address within {AddressBudget}");
        return addr;
    }

    /// <summary>Connects <paramref name="from"/> to <paramref name="to"/> by full address.</summary>
    public static async Task<Connection> ConnectAsync(Endpoint from, Endpoint to, string alpn, CancellationToken ct = default)
        => await from.ConnectWithAddrAsync(await AddrAsync(to, ct), alpn, ct);
}
