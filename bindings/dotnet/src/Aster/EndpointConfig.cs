using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

namespace Aster;

/// <summary>
/// Builder for EndpointConfig with a fluent API.
/// </summary>
public sealed class EndpointConfigBuilder
{
    private RelayMode _relayMode = RelayMode.Default;
    private byte[]? _secretKey;
    private readonly List<byte[]> _alpns = new();
    private bool _enableDiscovery = true;

    public EndpointConfigBuilder SetRelayMode(RelayMode mode) { _relayMode = mode; return this; }
    public EndpointConfigBuilder Alpn(string alpn) { _alpns.Add(Encoding.UTF8.GetBytes(alpn)); return this; }
    public EndpointConfigBuilder Alpns(IEnumerable<string> alpns)
    {
        foreach (var alpn in alpns) _alpns.Add(Encoding.UTF8.GetBytes(alpn));
        return this;
    }
    public EndpointConfigBuilder SecretKey(byte[] sk) { _secretKey = sk; return this; }
    public EndpointConfigBuilder EnableDiscovery(bool enabled) { _enableDiscovery = enabled; return this; }

    internal int AlpnCount => _alpns.Count;
    internal RelayMode RelayModeValue => _relayMode;
    internal bool EnableDiscoveryValue => _enableDiscovery;

    internal IEnumerable<(byte[] Bytes, int Len)> EnumerateAlpns()
    {
        foreach (var b in _alpns) yield return (b, b.Length);
    }
}
