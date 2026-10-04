using System;
using System.Collections.Generic;
using System.Linq;
using System.Net;
using System.Text;
using System.Text.Json;

namespace Aster;

/// <summary>
/// A compact <c>aster1&lt;base58&gt;</c> ticket: a peer's endpoint id plus the addresses (and
/// optional credential) needed to reach it. Encoded and decoded by Rust
/// (<c>aster_ticket_encode</c> / <c>aster_ticket_decode</c>).
/// </summary>
public sealed record AsterTicket(
    string EndpointId,
    string? RelayAddr = null,
    IReadOnlyList<string>? DirectAddrs = null,
    string? CredentialType = null,
    byte[]? CredentialData = null)
{
    public IReadOnlyList<string> DirectAddrs { get; init; } = DirectAddrs ?? Array.Empty<string>();

    /// <summary>The address to dial (relay addresses in tickets are <c>ip:port</c>, not URLs).</summary>
    public NodeAddrInfo ToNodeAddr() => new(EndpointId, null, DirectAddrs);

    /// <summary>A ticket for <paramref name="addr"/>: its direct addresses, and the relay if it is an <c>ip:port</c>.</summary>
    public static AsterTicket From(NodeAddrInfo addr)
        => new(addr.EndpointId, IsIpPort(addr.RelayUrl) ? addr.RelayUrl : null,
            addr.DirectAddresses.Where(a => IsIpPort(a)).ToList());

    public static unsafe AsterTicket Parse(string ticket)
    {
        ArgumentException.ThrowIfNullOrEmpty(ticket);
        byte[] input = Encoding.UTF8.GetBytes(ticket);
        byte[] output = new byte[8192];
        UIntPtr len = (UIntPtr)output.Length;
        int status;
        fixed (byte* i = input)
        fixed (byte* o = output)
            status = Native.aster_ticket_decode(i, (UIntPtr)input.Length, o, &len);
        if (status != 0)
            throw new FormatException($"invalid aster ticket: {IrohException.FromStatus(status, "aster_ticket_decode").Message}");

        using var doc = JsonDocument.Parse(output.AsMemory(0, (int)len));
        var root = doc.RootElement;
        string? Str(string key) => root.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
        var direct = new List<string>();
        if (root.TryGetProperty("direct_addrs", out var arr) && arr.ValueKind == JsonValueKind.Array)
            direct.AddRange(arr.EnumerateArray().Select(e => e.GetString()!).Where(s => s != null));
        string? credHex = Str("credential_data_hex");
        return new AsterTicket(Str("endpoint_id") ?? string.Empty, Str("relay_addr"), direct,
            Str("credential_type"), credHex is null ? null : Convert.FromHexString(credHex));
    }

    public static bool TryParse(string ticket, out AsterTicket? result)
    {
        try { result = Parse(ticket); return true; }
        catch (FormatException) { result = null; return false; }
    }

    /// <summary>The <c>aster1...</c> string.</summary>
    public override unsafe string ToString()
    {
        byte[] id = Encoding.UTF8.GetBytes(EndpointId);
        byte[] relay = RelayAddr is null ? Array.Empty<byte>() : Encoding.UTF8.GetBytes(RelayAddr);
        byte[] direct = DirectAddrs.Count == 0 ? Array.Empty<byte>() : JsonSerializer.SerializeToUtf8Bytes(DirectAddrs);
        byte[] credType = CredentialType is null ? Array.Empty<byte>() : Encoding.UTF8.GetBytes(CredentialType);
        byte[] credData = CredentialData ?? Array.Empty<byte>();
        byte[] output = new byte[4096];
        UIntPtr len = (UIntPtr)output.Length;
        int status;
        fixed (byte* pId = id)
        fixed (byte* pRelay = relay)
        fixed (byte* pDirect = direct)
        fixed (byte* pType = credType)
        fixed (byte* pData = credData)
        fixed (byte* o = output)
        {
            status = Native.aster_ticket_encode(
                pId, (UIntPtr)id.Length,
                relay.Length == 0 ? null : pRelay, (UIntPtr)relay.Length,
                direct.Length == 0 ? null : pDirect, (UIntPtr)direct.Length,
                credType.Length == 0 ? null : pType, (UIntPtr)credType.Length,
                credData.Length == 0 ? null : pData, (UIntPtr)credData.Length,
                o, &len);
        }
        Native.Check(status, "aster_ticket_encode");
        return Encoding.UTF8.GetString(output, 0, (int)len);
    }

    private static bool IsIpPort(string? s) => s != null && IPEndPoint.TryParse(s, out var ep) && ep.Port != 0;
}
