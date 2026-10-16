using System;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;

namespace Aster;

/// <summary>
/// Fluent configuration for <see cref="Endpoint.CreateAsync(Runtime, EndpointConfigBuilder, System.Threading.CancellationToken)"/>
/// (an in-memory node accepting these ALPNs).
/// </summary>
public sealed class EndpointConfigBuilder
{
    private RelayMode _relayMode = RelayMode.Default;
    private byte[]? _secretKey;
    private readonly List<byte[]> _alpns = new();
    private bool _enableDiscovery = true;
    private string? _bindAddr;

    public EndpointConfigBuilder SetRelayMode(RelayMode mode) { _relayMode = mode; return this; }
    public EndpointConfigBuilder Alpn(string alpn) { _alpns.Add(Encoding.UTF8.GetBytes(alpn)); return this; }
    public EndpointConfigBuilder Alpns(IEnumerable<string> alpns)
    {
        foreach (var alpn in alpns) _alpns.Add(Encoding.UTF8.GetBytes(alpn));
        return this;
    }
    public EndpointConfigBuilder SecretKey(byte[] sk) { _secretKey = sk; return this; }
    /// <summary>
    /// <c>false</c> turns off address lookup altogether: local mDNS and the n0 pkarr/DNS lookups
    /// (<see cref="DiscoveryMode.None"/>), so peers are dialled by full address
    /// (<see cref="Endpoint.ConnectWithAddrAsync"/>). <c>true</c> (the default) keeps n0 discovery and adds mDNS.
    /// </summary>
    public EndpointConfigBuilder EnableDiscovery(bool enabled) { _enableDiscovery = enabled; return this; }
    /// <summary>The IPv4 bind address, e.g. <c>127.0.0.1:0</c> (<see cref="AsterConfig.BindAddr"/>).</summary>
    public EndpointConfigBuilder BindAddr(string addr) { _bindAddr = addr; return this; }

    /// <summary>The <see cref="AsterConfig"/> of the in-memory node an endpoint starts (ABI 4.0).</summary>
    internal AsterConfig ToAsterConfig() => new()
    {
        RelayMode = _relayMode switch
        {
            RelayMode.Disabled => "disabled",
            RelayMode.Staging => "staging",
            RelayMode.Custom => "custom",
            _ => null,
        },
        SecretKey = _secretKey,
        Alpns = _alpns.Select(a => Encoding.UTF8.GetString(a)).ToArray(),
        LocalDiscovery = _enableDiscovery,
        Discovery = _enableDiscovery ? DiscoveryMode.Default : DiscoveryMode.None,
        BindAddr = _bindAddr,
    };
}
