using System;
using System.Collections.Generic;
using System.Linq;
using System.Text;

namespace Aster;

/// <summary>Gate-0 trust posture (<c>aster_trust_mode_t</c>).</summary>
public enum TrustMode : uint
{
    /// <summary>Unknown peers may use only the admission ALPNs (default).</summary>
    Protected = 0,
    /// <summary>Local/dev: unknown peers may use any ALPN.</summary>
    OpenDev = 1,
}

/// <summary>
/// Gate-0 connection admission policy, decided in Rust (<c>aster::GatePolicy</c>, C ABI
/// <c>aster_gate_*</c>). Pass it to <see cref="Node.StartAsync(AsterConfig?, Gate?, IEnumerable{string}?, System.Threading.CancellationToken)"/>
/// (or <see cref="Node.SetGate"/>): admission ALPNs stay open to everyone, every other ALPN
/// needs an <see cref="Admit">admitted</see> peer (or <see cref="TrustMode.OpenDev"/>), and
/// rejected peers are closed with the <see cref="SetReject">reject</see> code and reason.
///
/// A gate belongs to one <see cref="Aster.Runtime"/>; nodes gated by it run on that runtime.
/// Peers are 64-hex node ids (case-insensitive).
/// </summary>
public sealed class Gate : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private bool _disposed;

    private Gate(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>
    /// A PROTECTED gate (admission ALPNs open, peers otherwise rejected) on
    /// <paramref name="runtime"/>, or on a new runtime that the gate shares with the nodes it
    /// gates.
    /// </summary>
    public static Gate Create(Runtime? runtime = null)
    {
        var rt = runtime?.Retain() ?? new Runtime();
        try
        {
            Native.Check(Native.aster_gate_new(rt.Handle, out ulong handle), "aster_gate_new");
            return new Gate(rt, handle);
        }
        catch
        {
            rt.Release();
            throw;
        }
    }

    /// <summary>The runtime this gate (and every node it gates) lives on.</summary>
    public Runtime Runtime => _runtime;

    internal ulong Handle
    {
        get
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            return _handle;
        }
    }

    public Gate SetTrustMode(TrustMode mode)
    {
        Native.Check(Native.aster_gate_set_trust_mode(_runtime.Handle, Handle, (uint)mode), "aster_gate_set_trust_mode");
        return this;
    }

    /// <summary>Replaces the ALPNs that stay open to peers that are not admitted.</summary>
    public Gate SetAdmissionAlpns(IEnumerable<string> alpns)
    {
        using var scope = new NativeScope();
        Native.Check(Native.aster_gate_set_admission_alpns(_runtime.Handle, Handle, scope.Utf8List(alpns.ToList())),
            "aster_gate_set_admission_alpns");
        return this;
    }

    /// <summary>The QUIC close code and reason sent to rejected peers.</summary>
    public Gate SetReject(uint code, string reason)
    {
        using var scope = new NativeScope();
        Native.Check(Native.aster_gate_set_reject(_runtime.Handle, Handle, code, scope.Utf8(reason)), "aster_gate_set_reject");
        return this;
    }

    /// <summary>Admits <paramref name="peer"/> (64-hex node id).</summary>
    public void Admit(string peer) => PeerOp(peer, Native.aster_gate_admit, "aster_gate_admit");

    /// <summary>Revokes <paramref name="peer"/>'s admission (new connections are gated again).</summary>
    public void Revoke(string peer) => PeerOp(peer, Native.aster_gate_revoke, "aster_gate_revoke");

    public bool IsAdmitted(string peer)
    {
        using var scope = new NativeScope();
        Native.Check(Native.aster_gate_is_admitted(_runtime.Handle, Handle, scope.Utf8(peer), out uint admitted),
            "aster_gate_is_admitted");
        return admitted != 0;
    }

    /// <summary>Whether a connection from <paramref name="peer"/> on <paramref name="alpn"/> would be accepted.</summary>
    public bool ShouldAllow(string peer, string alpn)
    {
        using var scope = new NativeScope();
        Native.Check(Native.aster_gate_should_allow(_runtime.Handle, Handle, scope.Utf8(peer), scope.Utf8(alpn), out uint decision),
            "aster_gate_should_allow");
        return decision == (uint)HookDecision.Allow;
    }

    private void PeerOp(string peer, Func<ulong, ulong, Bytes, int> op, string name)
    {
        using var scope = new NativeScope();
        Native.Check(op(_runtime.Handle, Handle, scope.Bytes(Encoding.ASCII.GetBytes(peer))), name);
    }

    /// <summary>Frees the gate handle (nodes using it keep it until they close) and releases the runtime.</summary>
    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        Native.aster_gate_free(_runtime.Handle, _handle);
        _runtime.Release();
    }
}
