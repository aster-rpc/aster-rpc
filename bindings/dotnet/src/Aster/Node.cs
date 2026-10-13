using System;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Channels;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// An Aster node: one QUIC endpoint plus blobs, docs, gossip and tags, started from an
/// <see cref="AsterConfig"/> through the Rust <c>aster::Node::start</c> (<c>aster_node_start</c>).
/// </summary>
public sealed class Node : IDisposable, IAsyncDisposable
{
    /// <summary>Local passive/shared topology. This property does not expose an RPC.</summary>
    public TopologyView Topology => new(this);

    private readonly Runtime _runtime;
    private readonly NodeHandle _handle;
    private readonly bool _ownsRuntime;
    private readonly Channel<AdmissionRequest>? _admissions;
    private int _disposed;

    internal Node(Runtime runtime, NodeHandle handle, bool ownsRuntime = false, bool admissionEvents = false)
    {
        _runtime = runtime;
        _handle = handle;
        _ownsRuntime = ownsRuntime;
        if (admissionEvents)
        {
            _admissions = Channel.CreateUnbounded<AdmissionRequest>(new UnboundedChannelOptions { SingleWriter = true });
            runtime.RegisterInbound(handle.Value, OnInboundEvent);
        }
    }

    /// <summary>
    /// Starts a node from <paramref name="config"/> (null = Rust defaults) plus
    /// <paramref name="extraAlpns"/>. With a <paramref name="gate"/>, Gate 0 is decided in Rust and
    /// the node runs on the gate's runtime; without one, a node whose config has
    /// <see cref="AsterConfig.EnableHooks"/> raises <see cref="Admissions"/> that must be answered.
    /// </summary>
    public static async Task<Node> StartAsync(
        AsterConfig? config = null,
        Gate? gate = null,
        IEnumerable<string>? extraAlpns = null,
        CancellationToken cancellationToken = default)
    {
        config ??= new AsterConfig();
        Runtime runtime = gate != null ? gate.Runtime.Retain() : new Runtime();
        try
        {
            ulong opId = BeginStart(runtime, config, gate, extraAlpns);
            Event ev = await runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.NodeCreated)
                throw EventError.From(runtime, ev, "aster_node_start");
            return new Node(runtime, new NodeHandle(ev.handle), ownsRuntime: true,
                admissionEvents: gate == null && config.EnableHooks);
        }
        catch
        {
            runtime.Release();
            throw;
        }
    }

    /// <summary>
    /// Starts a node as <see cref="AsterServer"/> / <see cref="AsterClient"/> do
    /// (<c>aster_node_start_enrolled</c>): when <paramref name="config"/> enables enrollment, Rust
    /// validates the whole plan first (fail closed, naming the setting), starts the node with
    /// fail-closed hooks and <see cref="Aster.Enrollment.Alpn"/>, and installs Enrollment from the
    /// credential; otherwise this is a plain start with no gate.
    /// </summary>
    internal static async Task<Node> StartEnrolledAsync(AsterConfig config, uint side, IEnumerable<string>? extraAlpns,
        CancellationToken cancellationToken)
    {
        byte[] trust = config.TrustJsonBytes();
        var runtime = new Runtime();
        try
        {
            ulong opId = BeginStartEnrolled(runtime, config, trust, side, extraAlpns);
            Event ev = await runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.NodeCreated)
                throw EventError.From(runtime, ev, "aster_node_start_enrolled");
            ulong enrollment = ev.related;
            var node = new Node(runtime, new NodeHandle(ev.handle), ownsRuntime: true,
                admissionEvents: enrollment == 0 && config.EnableHooks);
            if (enrollment != 0) node.Enrollment = new Enrollment(runtime, enrollment);
            return node;
        }
        catch
        {
            runtime.Release();
            throw;
        }
    }

    /// <summary>
    /// The Enrollment installed when an <see cref="AsterServer"/> / <see cref="AsterClient"/>
    /// started this node from a config that enables it (null otherwise). It owns admission.
    /// </summary>
    public Enrollment? Enrollment { get; private set; }

    private static unsafe ulong BeginStartEnrolled(Runtime runtime, AsterConfig config, byte[] trust, uint side, IEnumerable<string>? extraAlpns)
    {
        using var scope = new NativeScope();
        ConfigNative native = config.ToNative(scope, extraAlpns);
        // The config and trust JSON are validated and copied before the call returns.
        Native.Check(Native.aster_node_start_enrolled(runtime.Handle, &native, scope.Bytes(trust), side, 0, out ulong opId),
            "aster_node_start_enrolled");
        return opId;
    }

    internal static unsafe ulong BeginStart(Runtime runtime, AsterConfig config, Gate? gate, IEnumerable<string>? extraAlpns)
    {
        using var scope = new NativeScope();
        ConfigNative native = config.ToNative(scope, extraAlpns);
        // The config is validated and copied before aster_node_start returns.
        Native.Check(Native.aster_node_start(runtime.Handle, &native, gate?.Handle ?? 0, 0, out ulong opId), "aster_node_start");
        return opId;
    }

    /// <summary>The runtime that owns this node.</summary>
    public Runtime Runtime => _runtime;

    /// <summary>The native node handle.</summary>
    public NodeHandle Handle => _handle;

    /// <summary>This node's id (64-hex Ed25519 public key).</summary>
    public string NodeId()
    {
        IntPtr buf = Marshal.AllocHGlobal(65);
        try
        {
            Native.Check(Native.aster_node_id(_runtime.Handle, _handle.Value, buf, (UIntPtr)65, out UIntPtr len), "aster_node_id");
            return len == UIntPtr.Zero ? string.Empty : Marshal.PtrToStringUTF8(buf, (int)len);
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    /// <summary>This node's address: id, relay URL and direct addresses.</summary>
    public NodeAddrInfo NodeAddr()
    {
        IntPtr buf = Marshal.AllocHGlobal(8192);
        try
        {
            Native.Check(Native.aster_node_addr_info(_runtime.Handle, _handle.Value, buf, (UIntPtr)8192, out NodeAddr addr),
                "aster_node_addr_info");
            return new NodeAddrInfo(
                addr.endpoint_id.ToUtf8String(),
                addr.relay_url.IsEmpty ? null : addr.relay_url.ToUtf8String(),
                NativeScope.ReadUtf8List(addr.direct_addresses));
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    /// <summary>
    /// An <c>aster1...</c> ticket for this node. Direct addresses can appear shortly after
    /// start; pass <paramref name="waitForDirectAddresses"/> to poll for them briefly.
    /// </summary>
    public async Task<AsterTicket> TicketAsync(TimeSpan? waitForDirectAddresses = null, CancellationToken cancellationToken = default)
    {
        var addr = NodeAddr();
        var deadline = DateTime.UtcNow + (waitForDirectAddresses ?? TimeSpan.Zero);
        while (addr.DirectAddresses.Count == 0 && DateTime.UtcNow < deadline)
        {
            await Task.Delay(50, cancellationToken).ConfigureAwait(false);
            addr = NodeAddr();
        }
        return AsterTicket.From(addr);
    }

    /// <summary>Connects to <paramref name="addr"/> on <paramref name="alpn"/>.</summary>
    public Task<Connection> ConnectAsync(NodeAddrInfo addr, string alpn, CancellationToken cancellationToken = default)
        => ConnectCore(_runtime, _handle.Value, addr, alpn, cancellationToken);

    /// <summary>Connects to the peer named by <paramref name="ticket"/> on <paramref name="alpn"/>.</summary>
    public Task<Connection> ConnectAsync(AsterTicket ticket, string alpn, CancellationToken cancellationToken = default)
        => ConnectAsync(ticket.ToNodeAddr(), alpn, cancellationToken);

    /// <summary>Teaches the node a peer's addresses (e.g. before a blob fetch by node id).</summary>
    public void AddNodeAddr(NodeAddrInfo addr)
    {
        using var scope = new NativeScope();
        var native = new NodeAddr
        {
            endpoint_id = scope.Utf8(addr.EndpointId),
            relay_url = scope.Utf8(addr.RelayUrl),
            direct_addresses = scope.Utf8List(addr.DirectAddresses),
        };
        Native.Check(Native.aster_add_node_addr(_runtime.Handle, _handle.Value, native), "aster_add_node_addr");
    }

    /// <summary>Accepts the next inbound connection on one of the node's ALPNs.</summary>
    public async Task<Connection> AcceptAsync(CancellationToken cancellationToken = default)
    {
        Native.Check(Native.aster_accept(_runtime.Handle, _handle.Value, 0, out ulong opId), "aster_accept");
        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.ConnectionAccepted)
            throw EventError.From(_runtime, ev, "aster_accept");
        return new Connection(_runtime, new ConnectionHandle(ev.handle));
    }

    internal static async Task<Connection> ConnectCore(Runtime runtime, ulong endpointOrNode, NodeAddrInfo addr, string alpn, CancellationToken ct)
    {
        ulong opId;
        unsafe
        {
            using var scope = new NativeScope();
            var nodeAddr = scope.Struct<NodeAddr>();
            nodeAddr->endpoint_id = scope.Utf8(addr.EndpointId);
            nodeAddr->relay_url = scope.Utf8(addr.RelayUrl);
            nodeAddr->direct_addresses = scope.Utf8List(addr.DirectAddresses);
            var config = scope.Struct<ConnectConfig>();
            config->struct_size = (uint)sizeof(ConnectConfig);
            config->node_id = scope.Utf8(addr.EndpointId);
            config->alpn = scope.Utf8(alpn);
            config->addr = (IntPtr)nodeAddr;
            // aster_connect copies the config before returning.
            Native.Check(Native.aster_connect(runtime.Handle, endpointOrNode, (IntPtr)config, 0, out opId), "aster_connect");
        }
        Event ev = await runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.Connected)
            throw EventError.From(runtime, ev, "aster_connect");
        return new Connection(runtime, new ConnectionHandle(ev.handle));
    }

    /// <summary>Attaches (or with <c>null</c> detaches) a Gate-0 policy. The gate must share this node's runtime.</summary>
    public void SetGate(Gate? gate)
    {
        if (gate != null && !ReferenceEquals(gate.Runtime, _runtime))
            throw new ArgumentException("the gate must be created on this node's runtime", nameof(gate));
        Native.Check(Native.aster_node_set_gate(_runtime.Handle, _handle.Value, gate?.Handle ?? 0), "aster_node_set_gate");
    }

    /// <summary>
    /// Prometheus text exposition of the process's metrics (<c>aster_metrics_prometheus</c>: the same
    /// text <c>/metrics</c> serves). The pipeline is process-wide, so this is not specific to the node.
    /// Null while no <see cref="Health.HealthServer"/> has given the pipeline a pull reader.
    /// </summary>
    public string? MetricsPrometheus() => Observability.AsterObservability.MetricsText();

    /// <summary>
    /// Admission events for a node started with <see cref="AsterConfig.EnableHooks"/> and no
    /// gate. Every request must be answered (<see cref="AdmissionRequest.Allow"/> /
    /// <see cref="AdmissionRequest.Deny"/>), otherwise the hook times out and the configured
    /// <see cref="AsterConfig.HookFailureMode"/> applies.
    /// </summary>
    public IAsyncEnumerable<AdmissionRequest> Admissions(CancellationToken cancellationToken = default)
        => (_admissions ?? throw new InvalidOperationException(
                "admission events need a node started with EnableHooks and no gate")).Reader.ReadAllAsync(cancellationToken);

    private void OnInboundEvent(Event ev)
    {
        var kind = (EventKind)ev.kind;
        if (kind != EventKind.HookBeforeConnect && kind != EventKind.HookAfterConnect) return;
        byte[] payload = ev.data_ptr == IntPtr.Zero || ev.data_len == UIntPtr.Zero ? Array.Empty<byte>() : new Bytes { ptr = ev.data_ptr, len = ev.data_len }.ToArray();
        var request = AdmissionRequest.Parse(_runtime, ev.related, kind == EventKind.HookAfterConnect, payload);
        if (!_admissions!.Writer.TryWrite(request))
            request.Allow();
    }

    /// <summary>The blob store.</summary>
    public Blobs Blobs() => new(_runtime, _handle.Value);

    /// <summary>Documents.</summary>
    public Docs Docs() => new(_runtime, _handle.Value);

    /// <summary>Gossip.</summary>
    public Gossip Gossip() => new(_runtime, _handle.Value);

    /// <summary>Blob tags.</summary>
    public Tags Tags() => new(_runtime, _handle.Value);

    /// <summary>Closes the node gracefully (<c>aster_node_close</c>) and releases its runtime.</summary>
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        Teardown();
        try
        {
            if (Native.aster_node_close(_runtime.Handle, _handle.Value, 0, out ulong opId) == 0)
                await _runtime.WaitForAsync(opId, TimeSpan.FromSeconds(10)).ConfigureAwait(false);
        }
        catch (OperationCanceledException) { /* best effort */ }
        finally
        {
            if (_ownsRuntime) _runtime.Release();
        }
    }

    /// <summary>Frees the node handle immediately and releases its runtime.</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        Teardown();
        Native.aster_node_free(_runtime.Handle, _handle.Value);
        if (_ownsRuntime) _runtime.Release();
    }

    private void Teardown()
    {
        // Closing the node releases its enrollment handle.
        Enrollment?.MarkClosed();
        if (_admissions != null)
        {
            _runtime.UnregisterInbound(_handle.Value);
            _admissions.Writer.TryComplete();
        }
    }
}

/// <summary>A peer address: endpoint id, optional relay URL, and direct <c>ip:port</c> addresses.</summary>
public sealed record NodeAddrInfo(string EndpointId, string? RelayUrl = null, IReadOnlyList<string>? DirectAddresses = null)
{
    public IReadOnlyList<string> DirectAddresses { get; init; } = DirectAddresses ?? Array.Empty<string>();
}

/// <summary>
/// A pending Gate-0 admission decision (<c>ASTER_EVENT_HOOK_BEFORE_CONNECT</c> for outbound
/// dials, <c>ASTER_EVENT_HOOK_AFTER_CONNECT</c> for inbound connections). Answer exactly once.
/// </summary>
public sealed class AdmissionRequest
{
    private readonly Runtime _runtime;
    private readonly ulong _invocation;
    private int _answered;

    private AdmissionRequest(Runtime runtime, ulong invocation, bool inbound, string peerId, string alpn, bool isAlive)
    {
        _runtime = runtime;
        _invocation = invocation;
        Inbound = inbound;
        PeerId = peerId;
        Alpn = alpn;
        IsAlive = isAlive;
    }

    /// <summary>True for an inbound connection (after handshake), false for an outbound dial.</summary>
    public bool Inbound { get; }

    /// <summary>The remote node id (64 hex).</summary>
    public string PeerId { get; }

    public string Alpn { get; }

    /// <summary>Inbound only: whether the connection was still alive when the event was raised.</summary>
    public bool IsAlive { get; }

    /// <summary>Accepts the connection.</summary>
    public void Allow() => Respond(HookDecision.Allow, 0, string.Empty);

    /// <summary>Rejects; an inbound connection is closed with <paramref name="code"/> and <paramref name="reason"/>.</summary>
    public void Deny(uint code = 403, string reason = "peer not admitted") => Respond(HookDecision.Deny, code, reason);

    private void Respond(HookDecision decision, uint code, string reason)
    {
        if (Interlocked.Exchange(ref _answered, 1) != 0)
            throw new InvalidOperationException("admission request already answered");
        using var scope = new NativeScope();
        int r = Native.aster_admission_respond(_runtime.Handle, _invocation, (uint)decision, code, scope.Utf8(reason));
        if (r != 0 && r != (int)Status.NotFound) Native.Check(r, "aster_admission_respond");
    }

    /// <summary>Payload: <c>[u32 LE id len][id][u32 LE alpn len][alpn]</c> (+ <c>[u8 is_alive]</c> after connect).</summary>
    internal static AdmissionRequest Parse(Runtime runtime, ulong invocation, bool inbound, byte[] p)
    {
        int off = 0;
        string Next()
        {
            if (p.Length < off + 4) return string.Empty;
            int n = (int)BitConverter.ToUInt32(p, off);
            off += 4;
            n = Math.Min(n, p.Length - off);
            string s = Encoding.UTF8.GetString(p, off, n);
            off += n;
            return s;
        }
        string peer = Next();
        string alpn = Next();
        bool alive = inbound && off < p.Length && p[off] != 0;
        return new AdmissionRequest(runtime, invocation, inbound, peer, alpn, alive);
    }
}

/// <summary>Turns a failed completion event into an exception carrying the native message.</summary>
internal static class EventError
{
    public static IrohException From(Runtime runtime, Event ev, string what)
    {
        string detail = string.Empty;
        if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
            detail = new Bytes { ptr = ev.data_ptr, len = ev.data_len }.ToUtf8String();
        if (ev.buffer != 0)
            Native.aster_buffer_release(runtime.Handle, ev.buffer);
        var status = ev.status != 0 ? (Status)ev.status : Status.Internal;
        string msg = $"{what}: {(EventKind)ev.kind} ({status})";
        return new IrohException(status, string.IsNullOrEmpty(detail) ? msg : $"{msg}: {detail}");
    }

    /// <summary>Copies an event's payload and releases its buffer.</summary>
    public static byte[] TakePayload(Runtime runtime, in Event ev)
    {
        byte[] data = ev.data_ptr == IntPtr.Zero || ev.data_len == UIntPtr.Zero
            ? Array.Empty<byte>()
            : new Bytes { ptr = ev.data_ptr, len = ev.data_len }.ToArray();
        if (ev.buffer != 0)
            Native.aster_buffer_release(runtime.Handle, ev.buffer);
        return data;
    }
}
