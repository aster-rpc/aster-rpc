using System;
using System.Collections.Generic;
using System.Text;
using Aster.Lease;

namespace Aster.Upcall;

/// <summary>A payload that is not the canonical encoding of the expected UCv1 message.</summary>
public sealed class Ucv1DecodeException : FormatException
{
    public Ucv1DecodeException(string message) : base("UCv1: " + message) { }
}

/// <summary>One HTTP header; a repeated name is one entry per value. <c>Name</c> is lowercase.</summary>
public sealed record HttpHeader(string Name, byte[] Value)
{
    public HttpHeader(string name, string value) : this(name, Encoding.UTF8.GetBytes(value)) { }

    /// <summary>The value as UTF-8 text.</summary>
    public string ValueText => Encoding.UTF8.GetString(Value);

    public bool Equals(HttpHeader? other) => other != null && Name == other.Name && Value.AsSpan().SequenceEqual(other.Value);
    public override int GetHashCode() => HashCode.Combine(Name, Value.Length);
}

/// <summary>Where an HTTP message body travels (R9).</summary>
public abstract record HttpBody
{
    private HttpBody() { }

    /// <summary>The whole body is in the message (possibly empty).</summary>
    public sealed record Inline(byte[] Body) : HttpBody
    {
        public bool Equals(Inline? other) => other != null && Body.AsSpan().SequenceEqual(other.Body);
        public override int GetHashCode() => Body.Length;
    }

    /// <summary>The body streams through the exchange's body handle (<c>stream_id</c>).</summary>
    public sealed record Streamed : HttpBody;

    public static readonly HttpBody Stream = new Streamed();
}

/// <summary>Kind 49 <c>expose_http</c> request (tag <c>C4</c>); <c>PeerId</c> is null off the relay.</summary>
public sealed record ExposeHttpRequestWire(
    string ServiceId, string? PeerId, ulong StreamId, byte Version, string Method, string Uri,
    IReadOnlyList<HttpHeader> Headers, HttpBody Body);

/// <summary>Kind 49 <c>expose_http</c> response head (tag <c>C5</c>); <c>Status</c> is 100..=999.</summary>
public sealed record ExposeHttpResponseWire(ushort Status, IReadOnlyList<HttpHeader> Headers, HttpBody Body);

/// <summary>A route spec as an edge's route policy sees it (<c>Protocol</c> 0 = http, 1 = tcp).</summary>
public sealed record ExposeRouteSpec(string Host, ushort Port, byte Protocol, string ServiceId)
{
    public const byte ProtocolHttp = 0;
    public const byte ProtocolTcp = 1;
}

/// <summary>Kind 50 request (tag <c>C7</c>).</summary>
public sealed record ExposeRoutePolicyRequest(string PeerId, byte[] Metadata, IReadOnlyList<ExposeRouteSpec> Specs);

/// <summary>Kind 48 request (tag <c>C0</c>).</summary>
public sealed record ExposeAuthorizeRequest(string ServiceId, string PeerId, byte[] Metadata);

public sealed record LeaseTransitionRequest(string Resource, LeaseOp Op);

public sealed record FencedMutateRequest(Fence Fence, string Writer, byte[] Op);

public sealed record ClusterView(IReadOnlyList<string> Members, string Bridge, IReadOnlyList<string> Witnesses);

public sealed record FarPair(string A, string B);

/// <summary>A serialized shared topology view (tag <c>BB</c>).</summary>
public sealed record TopologySnapshot(IReadOnlyList<ClusterView> Clusters, IReadOnlyList<FarPair> FarPairs, ulong SeparationCoverage);

public sealed record AdmissionConnectRequest(string Peer, byte[] Alpn);

public sealed record AdmissionHandshakeRequest(string Peer, byte[] Alpn, bool IsAlive);

/// <summary>Accept (code 0, empty reason) or reject with a code and reason.</summary>
public sealed record HandshakeDecision(bool Accept, uint Code, byte[] Reason);

/// <summary>
/// UCv1, the canonical upcall payload encoding (design <c>docs/_internal/bindings/upcall-design.md</c>
/// §3, vectors <c>conformance/vectors/upcalls.json</c>):
/// <code>
/// message := tag:u8 0x01 field*
/// uint    := LEB128 varint, minimal length, range-checked to the field width
/// u8enum  := one raw byte        bool := 0x00 | 0x01
/// string  := varint((utf8_len &lt;&lt; 2) | 2) utf8   (minimal header)
/// bytes   := varint(len) raw
/// opt&lt;T&gt;  := 0xFD (absent) | 0x00 T
/// list&lt;T&gt; := varint(count) 0x0C T{count}
/// </code>
/// Every field is present in a fixed order with no trailing bytes; decoders refuse anything else,
/// so each value has exactly one encoding. Covers every message the C ABI issues or answers:
/// leases, fenced mutation, decisions, topology, expose (C0, C4-C8) and admission. Placement
/// (<c>BC</c>..<c>BF</c>) is not implemented: the C ABI has no coordination client.
/// </summary>
public static class Ucv1
{
    public const byte Version = 0x01;

    public const byte TagLeaseTransitionRequest = 0xB1;
    public const byte TagTransitionOutcome = 0xB2;
    public const byte TagLeaseSnapshotRequest = 0xB3;
    public const byte TagLeaseSnapshot = 0xB4;
    public const byte TagLeaseAuthorizeRequest = 0xB5;
    public const byte TagFencedMutateRequest = 0xB6;
    public const byte TagFencedMutateResponse = 0xB7;
    public const byte TagTopologyAdmittedRequest = 0xB8;
    public const byte TagTopologyAdmittedResponse = 0xB9;
    public const byte TagTopologySnapshotRequest = 0xBA;
    public const byte TagTopologySnapshot = 0xBB;
    public const byte TagExposeAuthorizeRequest = 0xC0;
    public const byte TagAdmissionConnectRequest = 0xC1;
    public const byte TagAdmissionHandshakeRequest = 0xC2;
    public const byte TagAdmissionHandshakeResponse = 0xC3;
    public const byte TagExposeHttpRequest = 0xC4;
    public const byte TagExposeHttpResponse = 0xC5;
    public const byte TagExposeHttpTrailers = 0xC6;
    public const byte TagExposeRoutePolicyRequest = 0xC7;
    public const byte TagExposeRouteGrant = 0xC8;
    public const byte TagDecision = 0xCF;

    /// <summary>Highest HTTP version code (0 HTTP/0.9 .. 4 HTTP/3).</summary>
    public const byte HttpVersionMax = 4;

    private const byte NullFlag = 0xFD;
    private const byte ListHeader = 0x0C;

    private static readonly UTF8Encoding StrictUtf8 = new(false, true);

    // ── writer / reader ──────────────────────────────────────────────────────

    internal sealed class W
    {
        private readonly List<byte> _out = new();

        public W(byte tag) { _out.Add(tag); _out.Add(Version); }

        public W U8(int v) { _out.Add((byte)v); return this; }

        public W Uint(ulong v)
        {
            while (v >= 0x80) { _out.Add((byte)((v & 0x7F) | 0x80)); v >>= 7; }
            _out.Add((byte)v);
            return this;
        }

        public W Bool(bool v) => U8(v ? 1 : 0);

        public W Str(string s)
        {
            byte[] b = StrictUtf8.GetBytes(s);
            Uint(((ulong)b.Length << 2) | 2);
            _out.AddRange(b);
            return this;
        }

        public W Bytes(byte[] b) { Uint((ulong)b.Length); _out.AddRange(b); return this; }

        public W List(int n) { Uint((ulong)n); return U8(ListHeader); }

        public W Absent() => U8(NullFlag);
        public W Present() => U8(0);

        public W Strs(IReadOnlyList<string> v)
        {
            List(v.Count);
            foreach (string s in v) Str(s);
            return this;
        }

        public byte[] Done() => _out.ToArray();
    }

    internal sealed class R
    {
        private readonly byte[] _buf;
        private int _pos;

        public R(byte[] buf, byte tag)
        {
            _buf = buf;
            if (buf.Length == 0) throw new Ucv1DecodeException("empty payload");
            if (buf[0] != tag) throw new Ucv1DecodeException($"tag 0x{buf[0]:x2}, expected 0x{tag:x2}");
            if (buf.Length < 2) throw new Ucv1DecodeException("missing version");
            if (buf[1] != Version) throw new Ucv1DecodeException($"unsupported version 0x{buf[1]:x2}");
            _pos = 2;
        }

        private void Need(ulong n)
        {
            if (n > (ulong)(_buf.Length - _pos)) throw new Ucv1DecodeException($"unexpected EOF at offset {_pos}");
        }

        public byte U8()
        {
            Need(1);
            return _buf[_pos++];
        }

        /// <summary>Unsigned LEB128, minimal, at most 64 bits.</summary>
        public ulong Uint()
        {
            int start = _pos;
            ulong v = 0;
            for (int i = 0; i < 10; i++)
            {
                byte b = U8();
                if (i == 9 && b > 1) throw new Ucv1DecodeException($"varint overflows 64 bits at offset {start}");
                v |= (ulong)(b & 0x7F) << (7 * i);
                if ((b & 0x80) == 0)
                {
                    if (i > 0 && b == 0) throw new Ucv1DecodeException($"non-minimal varint at offset {start}");
                    return v;
                }
            }
            throw new Ucv1DecodeException($"varint too long at offset {start}");
        }

        public ulong UintMax(ulong max, string what)
        {
            ulong v = Uint();
            if (v > max) throw new Ucv1DecodeException($"{what} {v} out of range");
            return v;
        }

        public bool Bool() => U8() switch
        {
            0 => false,
            1 => true,
            var b => throw new Ucv1DecodeException($"invalid bool byte {b}"),
        };

        public string Str()
        {
            int start = _pos;
            ulong header = Uint();
            if ((header & 0b11) != 0b10) throw new Ucv1DecodeException($"string header has wrong encoding bits at offset {start}");
            ulong len = header >> 2;
            Need(len);
            try
            {
                string s = StrictUtf8.GetString(_buf, _pos, (int)len);
                _pos += (int)len;
                return s;
            }
            catch (DecoderFallbackException)
            {
                throw new Ucv1DecodeException($"invalid UTF-8 at offset {start}");
            }
        }

        public byte[] Bytes()
        {
            ulong len = Uint();
            Need(len);
            byte[] b = _buf.AsSpan(_pos, (int)len).ToArray();
            _pos += (int)len;
            return b;
        }

        public int List()
        {
            ulong n = Uint();
            if (U8() != ListHeader) throw new Ucv1DecodeException("list element header mismatch");
            if (n > (ulong)(_buf.Length - _pos)) throw new Ucv1DecodeException($"list count {n} exceeds payload");
            return (int)n;
        }

        public bool Present()
        {
            byte b = U8();
            if (b == 0) return true;
            if (b == NullFlag) return false;
            throw new Ucv1DecodeException($"invalid optional marker {b}");
        }

        public List<string> Strs()
        {
            int n = List();
            var o = new List<string>(n);
            for (int i = 0; i < n; i++) o.Add(Str());
            return o;
        }

        public void End()
        {
            if (_pos != _buf.Length) throw new Ucv1DecodeException($"{_buf.Length - _pos} trailing bytes");
        }
    }

    // ── leases ───────────────────────────────────────────────────────────────

    private static void PutOp(W w, LeaseOp op)
    {
        switch (op)
        {
            case LeaseOp.Acquire a: w.U8(1).Str(a.Candidate).Uint(a.TtlMs); break;
            case LeaseOp.Renew r: w.U8(2).Str(r.Holder).Uint(r.Epoch); break;
            case LeaseOp.Release r: w.U8(3).Str(r.Holder).Uint(r.Epoch); break;
            case LeaseOp.Revoke r: w.U8(4).Str(r.Authority); break;
            default: throw new ArgumentException("unknown lease op", nameof(op));
        }
    }

    private static LeaseOp GetOp(R r)
    {
        byte op = r.U8();
        return op switch
        {
            1 => new LeaseOp.Acquire(r.Str(), r.Uint()),
            2 => new LeaseOp.Renew(r.Str(), r.Uint()),
            3 => new LeaseOp.Release(r.Str(), r.Uint()),
            4 => new LeaseOp.Revoke(r.Str()),
            _ => throw new Ucv1DecodeException($"unknown lease op {op}"),
        };
    }

    private static void PutFence(W w, Fence f) => w.Str(f.Resource).Uint(f.Epoch).Str(f.Holder);

    private static Fence GetFence(R r) => new(r.Str(), r.Uint(), r.Str());

    private static void PutSnapshot(W w, LeaseSnapshot s)
    {
        w.Str(s.Resource).Uint(s.Epoch);
        if (s.Holder == null) w.Absent(); else w.Present().Str(s.Holder);
        w.Bool(s.Held);
        if (s.RemainingMs == null) w.Absent(); else w.Present().Uint(s.RemainingMs.Value);
    }

    private static LeaseSnapshot GetSnapshot(R r)
    {
        string resource = r.Str();
        ulong epoch = r.Uint();
        string? holder = r.Present() ? r.Str() : null;
        bool held = r.Bool();
        ulong? remaining = r.Present() ? r.Uint() : null;
        return new LeaseSnapshot(resource, epoch, holder, held, remaining);
    }

    public static byte[] EncodeLeaseTransitionRequest(string resource, LeaseOp op)
    {
        var w = new W(TagLeaseTransitionRequest).Str(resource);
        PutOp(w, op);
        return w.Done();
    }

    public static LeaseTransitionRequest DecodeLeaseTransitionRequest(byte[] buf)
    {
        var r = new R(buf, TagLeaseTransitionRequest);
        var v = new LeaseTransitionRequest(r.Str(), GetOp(r));
        r.End();
        return v;
    }

    public static byte[] EncodeTransitionOutcome(TransitionOutcome o)
    {
        var w = new W(TagTransitionOutcome);
        switch (o)
        {
            case TransitionOutcome.Granted g: w.U8(1); PutFence(w, g.Fence); break;
            case TransitionOutcome.Renewed: w.U8(2); break;
            case TransitionOutcome.Released: w.U8(3); break;
            case TransitionOutcome.Revoked: w.U8(4); break;
            case TransitionOutcome.Denied d: w.U8(5).U8((byte)d.Reason); PutSnapshot(w, d.Snapshot); break;
            default: throw new ArgumentException("unknown outcome", nameof(o));
        }
        return w.Done();
    }

    public static TransitionOutcome DecodeTransitionOutcome(byte[] buf)
    {
        var r = new R(buf, TagTransitionOutcome);
        byte kind = r.U8();
        TransitionOutcome o;
        switch (kind)
        {
            case 1: o = new TransitionOutcome.Granted(GetFence(r)); break;
            case 2: o = new TransitionOutcome.Renewed(); break;
            case 3: o = new TransitionOutcome.Released(); break;
            case 4: o = new TransitionOutcome.Revoked(); break;
            case 5:
                byte code = r.U8();
                if (code < 1 || code > 6) throw new Ucv1DecodeException($"unknown deny reason {code}");
                o = new TransitionOutcome.Denied((DenyReason)code, GetSnapshot(r));
                break;
            default: throw new Ucv1DecodeException($"unknown transition outcome {kind}");
        }
        r.End();
        return o;
    }

    public static byte[] EncodeLeaseSnapshotRequest(string resource) => new W(TagLeaseSnapshotRequest).Str(resource).Done();

    public static string DecodeLeaseSnapshotRequest(byte[] buf)
    {
        var r = new R(buf, TagLeaseSnapshotRequest);
        string s = r.Str();
        r.End();
        return s;
    }

    public static byte[] EncodeLeaseSnapshot(LeaseSnapshot s)
    {
        var w = new W(TagLeaseSnapshot);
        PutSnapshot(w, s);
        return w.Done();
    }

    public static LeaseSnapshot DecodeLeaseSnapshot(byte[] buf)
    {
        var r = new R(buf, TagLeaseSnapshot);
        var s = GetSnapshot(r);
        r.End();
        return s;
    }

    public static byte[] EncodeLeaseAuthorizeRequest(LeaseOp op)
    {
        var w = new W(TagLeaseAuthorizeRequest);
        PutOp(w, op);
        return w.Done();
    }

    public static LeaseOp DecodeLeaseAuthorizeRequest(byte[] buf)
    {
        var r = new R(buf, TagLeaseAuthorizeRequest);
        var op = GetOp(r);
        r.End();
        return op;
    }

    public static byte[] EncodeFencedMutateRequest(Fence fence, string writer, byte[] op)
    {
        var w = new W(TagFencedMutateRequest);
        PutFence(w, fence);
        return w.Str(writer).Bytes(op).Done();
    }

    public static FencedMutateRequest DecodeFencedMutateRequest(byte[] buf)
    {
        var r = new R(buf, TagFencedMutateRequest);
        var v = new FencedMutateRequest(GetFence(r), r.Str(), r.Bytes());
        r.End();
        return v;
    }

    public static byte[] EncodeFencedMutateResponse(MutateResult res)
    {
        var w = new W(TagFencedMutateResponse);
        switch (res)
        {
            case MutateResult.Applied a: w.U8(0).Bytes(a.Output); break;
            case MutateResult.Rejected x:
                var rej = x.Rejection;
                bool epochs = rej.Reason is FenceRejectionReason.StaleEpoch or FenceRejectionReason.FutureEpoch;
                w.U8(1).U8((byte)rej.Reason).Uint(epochs ? rej.Carried : 0).Uint(epochs ? rej.Current : 0);
                break;
            default: throw new ArgumentException("unknown result", nameof(res));
        }
        return w.Done();
    }

    public static MutateResult DecodeFencedMutateResponse(byte[] buf)
    {
        var r = new R(buf, TagFencedMutateResponse);
        byte kind = r.U8();
        MutateResult o;
        if (kind == 0)
        {
            o = new MutateResult.Applied(r.Bytes());
        }
        else if (kind == 1)
        {
            byte code = r.U8();
            ulong carried = r.Uint();
            ulong current = r.Uint();
            if (code < 1 || code > 4) throw new Ucv1DecodeException($"unknown fence rejection {code}");
            var reason = (FenceRejectionReason)code;
            if (reason is FenceRejectionReason.NotHolder or FenceRejectionReason.NotHeld && (carried != 0 || current != 0))
                throw new Ucv1DecodeException($"epochs must be 0 for rejection {code}");
            o = new MutateResult.Rejected(new FenceRejection(reason, carried, current));
        }
        else
        {
            throw new Ucv1DecodeException($"unknown fenced-mutate result {kind}");
        }
        r.End();
        return o;
    }

    // ── decisions ────────────────────────────────────────────────────────────

    public static byte[] EncodeDecision(bool allowed) => new W(TagDecision).Bool(allowed).Done();

    public static bool DecodeDecision(byte[] buf)
    {
        var r = new R(buf, TagDecision);
        bool v = r.Bool();
        r.End();
        return v;
    }

    // ── topology ─────────────────────────────────────────────────────────────

    public static byte[] EncodeTopologyAdmittedRequest(IReadOnlyList<string> nodeIds) => new W(TagTopologyAdmittedRequest).Strs(nodeIds).Done();

    public static List<string> DecodeTopologyAdmittedRequest(byte[] buf)
    {
        var r = new R(buf, TagTopologyAdmittedRequest);
        var v = r.Strs();
        r.End();
        return v;
    }

    public static byte[] EncodeTopologyAdmittedResponse(IReadOnlyList<bool> admitted)
    {
        var w = new W(TagTopologyAdmittedResponse).List(admitted.Count);
        foreach (bool b in admitted) w.Bool(b);
        return w.Done();
    }

    public static List<bool> DecodeTopologyAdmittedResponse(byte[] buf)
    {
        var r = new R(buf, TagTopologyAdmittedResponse);
        int n = r.List();
        var v = new List<bool>(n);
        for (int i = 0; i < n; i++) v.Add(r.Bool());
        r.End();
        return v;
    }

    public static byte[] EncodeTopologySnapshotRequest() => new W(TagTopologySnapshotRequest).Done();

    public static void DecodeTopologySnapshotRequest(byte[] buf) => new R(buf, TagTopologySnapshotRequest).End();

    public static byte[] EncodeTopologySnapshot(TopologySnapshot v)
    {
        var w = new W(TagTopologySnapshot).List(v.Clusters.Count);
        foreach (var c in v.Clusters) w.Strs(c.Members).Str(c.Bridge).Strs(c.Witnesses);
        w.List(v.FarPairs.Count);
        foreach (var p in v.FarPairs) w.Str(p.A).Str(p.B);
        return w.Uint(v.SeparationCoverage).Done();
    }

    public static TopologySnapshot DecodeTopologySnapshot(byte[] buf)
    {
        var r = new R(buf, TagTopologySnapshot);
        int n = r.List();
        var clusters = new List<ClusterView>(n);
        for (int i = 0; i < n; i++) clusters.Add(new ClusterView(r.Strs(), r.Str(), r.Strs()));
        int m = r.List();
        var pairs = new List<FarPair>(m);
        for (int i = 0; i < m; i++) pairs.Add(new FarPair(r.Str(), r.Str()));
        ulong coverage = r.Uint();
        r.End();
        return new TopologySnapshot(clusters, pairs, coverage);
    }

    // ── expose / admission ───────────────────────────────────────────────────

    public static byte[] EncodeExposeAuthorizeRequest(ExposeAuthorizeRequest v)
        => new W(TagExposeAuthorizeRequest).Str(v.ServiceId).Str(v.PeerId).Bytes(v.Metadata).Done();

    public static ExposeAuthorizeRequest DecodeExposeAuthorizeRequest(byte[] buf)
    {
        var r = new R(buf, TagExposeAuthorizeRequest);
        var v = new ExposeAuthorizeRequest(r.Str(), r.Str(), r.Bytes());
        r.End();
        return v;
    }

    public static byte[] EncodeAdmissionConnectRequest(AdmissionConnectRequest v)
        => new W(TagAdmissionConnectRequest).Str(v.Peer).Bytes(v.Alpn).Done();

    public static AdmissionConnectRequest DecodeAdmissionConnectRequest(byte[] buf)
    {
        var r = new R(buf, TagAdmissionConnectRequest);
        var v = new AdmissionConnectRequest(r.Str(), r.Bytes());
        r.End();
        return v;
    }

    public static byte[] EncodeAdmissionHandshakeRequest(AdmissionHandshakeRequest v)
        => new W(TagAdmissionHandshakeRequest).Str(v.Peer).Bytes(v.Alpn).Bool(v.IsAlive).Done();

    public static AdmissionHandshakeRequest DecodeAdmissionHandshakeRequest(byte[] buf)
    {
        var r = new R(buf, TagAdmissionHandshakeRequest);
        var v = new AdmissionHandshakeRequest(r.Str(), r.Bytes(), r.Bool());
        r.End();
        return v;
    }

    public static byte[] EncodeAdmissionHandshakeResponse(HandshakeDecision d)
        => d.Accept
            ? new W(TagAdmissionHandshakeResponse).Bool(true).Uint(0).Bytes(Array.Empty<byte>()).Done()
            : new W(TagAdmissionHandshakeResponse).Bool(false).Uint(d.Code).Bytes(d.Reason).Done();

    public static HandshakeDecision DecodeAdmissionHandshakeResponse(byte[] buf)
    {
        var r = new R(buf, TagAdmissionHandshakeResponse);
        bool accept = r.Bool();
        uint code = (uint)r.UintMax(0xFFFF_FFFF, "reject code");
        byte[] reason = r.Bytes();
        r.End();
        if (accept && (code != 0 || reason.Length != 0)) throw new Ucv1DecodeException("accept carries no code or reason");
        return new HandshakeDecision(accept, code, reason);
    }

    // ── expose HTTP / route policy (R9) ──────────────────────────────────────

    private static void PutHeaders(W w, IReadOnlyList<HttpHeader> headers)
    {
        w.List(headers.Count);
        foreach (var h in headers) w.Str(h.Name).Bytes(h.Value);
    }

    private static List<HttpHeader> GetHeaders(R r)
    {
        int n = r.List();
        var o = new List<HttpHeader>(n);
        for (int i = 0; i < n; i++)
        {
            string name = r.Str();
            if (name.Length == 0 || name.AsSpan().IndexOfAnyInRange('A', 'Z') >= 0)
                throw new Ucv1DecodeException($"header name \"{name}\" must be non-empty and lowercase");
            o.Add(new HttpHeader(name, r.Bytes()));
        }
        return o;
    }

    private static void PutBody(W w, HttpBody body)
    {
        switch (body)
        {
            case HttpBody.Inline i: w.U8(0).Bytes(i.Body); break;
            case HttpBody.Streamed: w.U8(1); break;
            default: throw new ArgumentException("unknown body", nameof(body));
        }
    }

    private static HttpBody GetBody(R r)
    {
        byte mode = r.U8();
        return mode switch
        {
            0 => new HttpBody.Inline(r.Bytes()),
            1 => HttpBody.Stream,
            _ => throw new Ucv1DecodeException($"unknown body mode {mode}"),
        };
    }

    public static byte[] EncodeExposeHttpRequest(ExposeHttpRequestWire v)
    {
        var w = new W(TagExposeHttpRequest).Str(v.ServiceId);
        if (v.PeerId == null) w.Absent(); else w.Present().Str(v.PeerId);
        w.Uint(v.StreamId).U8(v.Version).Str(v.Method).Str(v.Uri);
        PutHeaders(w, v.Headers);
        PutBody(w, v.Body);
        return w.Done();
    }

    public static ExposeHttpRequestWire DecodeExposeHttpRequest(byte[] buf)
    {
        var r = new R(buf, TagExposeHttpRequest);
        string serviceId = r.Str();
        string? peerId = r.Present() ? r.Str() : null;
        ulong streamId = r.Uint();
        byte version = r.U8();
        if (version > HttpVersionMax) throw new Ucv1DecodeException($"unknown HTTP version {version}");
        string method = r.Str();
        if (method.Length == 0) throw new Ucv1DecodeException("empty HTTP method");
        string uri = r.Str();
        var headers = GetHeaders(r);
        var body = GetBody(r);
        r.End();
        return new ExposeHttpRequestWire(serviceId, peerId, streamId, version, method, uri, headers, body);
    }

    public static byte[] EncodeExposeHttpResponse(ExposeHttpResponseWire v)
    {
        var w = new W(TagExposeHttpResponse).Uint(v.Status);
        PutHeaders(w, v.Headers);
        PutBody(w, v.Body);
        return w.Done();
    }

    public static ExposeHttpResponseWire DecodeExposeHttpResponse(byte[] buf)
    {
        var r = new R(buf, TagExposeHttpResponse);
        ushort status = (ushort)r.UintMax(999, "HTTP status");
        if (status < 100) throw new Ucv1DecodeException($"HTTP status {status} below 100");
        var headers = GetHeaders(r);
        var body = GetBody(r);
        r.End();
        return new ExposeHttpResponseWire(status, headers, body);
    }

    public static byte[] EncodeExposeHttpTrailers(IReadOnlyList<HttpHeader> trailers)
    {
        var w = new W(TagExposeHttpTrailers);
        PutHeaders(w, trailers);
        return w.Done();
    }

    public static List<HttpHeader> DecodeExposeHttpTrailers(byte[] buf)
    {
        var r = new R(buf, TagExposeHttpTrailers);
        var v = GetHeaders(r);
        r.End();
        return v;
    }

    public static byte[] EncodeExposeRoutePolicyRequest(ExposeRoutePolicyRequest v)
    {
        var w = new W(TagExposeRoutePolicyRequest).Str(v.PeerId).Bytes(v.Metadata);
        w.List(v.Specs.Count);
        foreach (var s in v.Specs) w.Str(s.Host).Uint(s.Port).U8(s.Protocol).Str(s.ServiceId);
        return w.Done();
    }

    public static ExposeRoutePolicyRequest DecodeExposeRoutePolicyRequest(byte[] buf)
    {
        var r = new R(buf, TagExposeRoutePolicyRequest);
        string peerId = r.Str();
        byte[] metadata = r.Bytes();
        int n = r.List();
        var specs = new List<ExposeRouteSpec>(n);
        for (int i = 0; i < n; i++)
        {
            string host = r.Str();
            ushort port = (ushort)r.UintMax(0xFFFF, "port");
            byte protocol = r.U8();
            if (protocol > ExposeRouteSpec.ProtocolTcp) throw new Ucv1DecodeException($"unknown route protocol {protocol}");
            specs.Add(new ExposeRouteSpec(host, port, protocol, r.Str()));
        }
        r.End();
        return new ExposeRoutePolicyRequest(peerId, metadata, specs);
    }

    /// <summary>The granted subset of a route request: indexes into its specs, strictly ascending.</summary>
    public static byte[] EncodeExposeRouteGrant(IReadOnlyList<uint> granted)
    {
        var w = new W(TagExposeRouteGrant).List(granted.Count);
        foreach (uint i in granted) w.Uint(i);
        return w.Done();
    }

    public static List<uint> DecodeExposeRouteGrant(byte[] buf)
    {
        var r = new R(buf, TagExposeRouteGrant);
        int n = r.List();
        var v = new List<uint>(n);
        for (int i = 0; i < n; i++)
        {
            uint idx = (uint)r.UintMax(0xFFFF_FFFF, "route index");
            if (v.Count > 0 && v[^1] >= idx) throw new Ucv1DecodeException("granted route indexes not strictly ascending");
            v.Add(idx);
        }
        r.End();
        return v;
    }
}
