using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Codec;
using Aster.Health;
using Aster.Interceptors;
using Aster.Observability;
using Aster.Rpc;

namespace Aster;

/// <summary>
/// An Aster RPC server on dynamic dispatch (C ABI 3.5, <c>aster_server_*</c>): starts a
/// <see cref="Aster.Node"/> from an <see cref="AsterConfig"/> and registers each
/// <see cref="ServiceDefinition"/> as its JSON registration form. Rust's <c>aster::rpc::Server</c>
/// decodes headers, routes, enforces Gate 3 (<see cref="CapabilityRequirement"/>), keeps sessions
/// per connection (cap, graveyard), enforces deadlines and per-method frame limits, and serves
/// <c>aster.ops.NodeInfo</c>. This class drains the dispatch events on one thread, runs each call
/// on the thread pool, runs the interceptors, and answers by call id with payloads and AEv1
/// <c>RpcStatus</c>. A handler exception becomes <c>INTERNAL</c> (or the code of an
/// <see cref="RpcError"/>).
/// <code>
/// await using var server = await AsterServer.StartAsync(new AsterServerOptions
/// {
///     Config = AsterConfig.FromEnv() with { AllowAllConsumers = true },
///     Services = { service },
/// });
/// Console.WriteLine(await server.TicketAsync());
/// </code>
/// </summary>
public sealed class AsterServer : IAsyncDisposable, IDisposable
{
    public const string AsterAlpn = "aster/1";
    /// <summary>The consumer-admission ALPN; kept open by the default gate.</summary>
    public const string ConsumerAdmissionAlpn = "aster.consumer_admission";

    private const int PollBatch = 64;
    private const uint PollTimeoutMs = 100;

    private readonly Node _node;
    private readonly Gate? _ownedGate;
    private readonly DispatchServer _dispatch;
    private readonly HealthServer? _health;
    private readonly ICodec _codec;
    private readonly Dictionary<string, ServiceDefinition> _services;
    private readonly ServiceDefinition[] _byIndex;
    private readonly IReadOnlyList<IInterceptor> _interceptors;
    private readonly Thread _pollThread;
    private volatile bool _running = true;
    private int _inFlight;
    private int _disposed;

    /// <summary>Live calls by call id: cancelled on a <c>CANCELLED</c> event or at shutdown.</summary>
    private readonly ConcurrentDictionary<ulong, CancellationTokenSource> _live = new();

    /// <summary>Session state by Rust's session key; created and dropped on the poll thread.</summary>
    private readonly ConcurrentDictionary<ulong, object?> _sessions = new();

    /// <summary>Peer id strings by connection id (poll thread only), so calls do not re-decode them.</summary>
    private readonly Dictionary<ulong, string> _peers = new();

    /// <summary>Attributes set through <see cref="SetPeerAttributes"/> (mirrored into Rust's Gate 3 store).</summary>
    private readonly ConcurrentDictionary<string, IReadOnlyDictionary<string, string>> _peerAttributes = new(StringComparer.Ordinal);

    private static readonly IReadOnlyDictionary<string, string> NoAttributes = new Dictionary<string, string>();

    private AsterServer(Node node, Gate? ownedGate, Gate gate, DispatchServer dispatch, HealthServer? health, ICodec codec,
        Dictionary<string, ServiceDefinition> services, ServiceDefinition[] byIndex, IReadOnlyList<IInterceptor> interceptors)
    {
        _interceptors = interceptors;
        _node = node;
        _ownedGate = ownedGate;
        Gate = gate;
        _dispatch = dispatch;
        _health = health;
        _codec = codec;
        _services = services;
        _byIndex = byIndex;
        _pollThread = new Thread(PollLoop) { Name = "aster-server-poll", IsBackground = true };
        _pollThread.Start();
    }

    /// <summary>Starts a server.</summary>
    public static async Task<AsterServer> StartAsync(AsterServerOptions options, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(options);
        if (options.MaxSessionsPerConnection < 1)
            throw new ArgumentOutOfRangeException(nameof(options), "MaxSessionsPerConnection must be >= 1");
        var services = new Dictionary<string, ServiceDefinition>(StringComparer.Ordinal);
        foreach (var s in options.Services)
            if (!services.TryAdd(s.Name, s))
                throw new ArgumentException($"service {s.Name} registered twice");

        AsterObservability.EnsureStarted();

        ICodec codec = options.Codec ?? new ForyCodec();
        if (codec is ForyCodec fory)
            foreach (var m in services.Values.SelectMany(s => s.Methods.Values))
                m.RegisterTypes(fory);

        var config = options.Config ?? new AsterConfig();
        Gate? ownedGate = null;
        Gate gate = options.Gate ?? (ownedGate = Gate.Create()
            .SetTrustMode(config.AllowAllConsumers ? TrustMode.OpenDev : TrustMode.Protected)
            .SetAdmissionAlpns([ConsumerAdmissionAlpn]));
        Node? node = null;
        HealthServer? health = null;
        try
        {
            node = await Node.StartAsync(config, gate, [AsterAlpn, ConsumerAdmissionAlpn], cancellationToken).ConfigureAwait(false);
            var byIndex = new ServiceDefinition[services.Count];
            if (options.Health is { } healthOptions)
                health = await HealthServer.StartAsync(node, healthOptions).ConfigureAwait(false);
            DispatchServer dispatch;
            using (var builder = new DispatchServer.Builder(node.Runtime.Handle, node.Handle.Value))
            {
                foreach (var s in services.Values)
                    byIndex[builder.AddServiceJson(s.RegistrationJson)] = s;
                if (health is not null) builder.AttachHealth(health.Handle);
                builder.Option(DispatchServerOption.MaxSessionsPerConnection, (ulong)options.MaxSessionsPerConnection);
                if (options.HandlerTimeout is { } cap)
                    builder.Option(DispatchServerOption.HandlerTimeoutMs, cap <= TimeSpan.Zero ? 0 : (ulong)Math.Ceiling(cap.TotalMilliseconds));
                dispatch = builder.Start(options.RingCapacity);
            }
            return new AsterServer(node, ownedGate, gate, dispatch, health, codec, services, byIndex, options.Interceptors.ToList());
        }
        catch
        {
            health?.Dispose();
            node?.Dispose();
            ownedGate?.Dispose();
            throw;
        }
    }

    /// <summary>The server's node.</summary>
    public Node Node => _node;

    /// <summary>The ops listener started from <see cref="AsterServerOptions.Health"/>, or null.</summary>
    public HealthServer? Health => _health;

    /// <summary>The Gate-0 policy guarding the node (admit peers here unless dev mode is on).</summary>
    public Gate Gate { get; }

    /// <summary>The server's node id (64 hex).</summary>
    public string NodeId() => _node.NodeId();

    /// <summary>A ticket for this server (waits briefly for direct addresses).</summary>
    public Task<AsterTicket> TicketAsync(CancellationToken cancellationToken = default)
        => _node.TicketAsync(TimeSpan.FromSeconds(5), cancellationToken);

    /// <summary>The registered services by name.</summary>
    public IReadOnlyDictionary<string, ServiceDefinition> Services => _services;

    /// <summary>Calls in Rust's call table (diagnostics / tests).</summary>
    public ulong LiveCalls => _dispatch.LiveCalls;

    /// <summary>Live session states (diagnostics / tests).</summary>
    public int ActiveSessions => _sessions.Count;

    /// <summary>
    /// Raised (on the poll thread) when an <c>aster/1</c> connection closed, after its sessions'
    /// <see cref="SessionClosed"/>. Per-connection state keyed on
    /// <see cref="ServerCallContext.ConnectionId"/> can be dropped here.
    /// </summary>
    public event Action<ulong>? ConnectionClosed;

    /// <summary>
    /// Raised (on the poll thread) when Rust reaps a session (<see cref="ServerCallContext.SessionKey"/>),
    /// after its state was disposed.
    /// </summary>
    public event Action<ulong>? SessionClosed;

    /// <summary>
    /// Raised (on the poll thread) for a connection on one of the node's non-RPC ALPNs (e.g.
    /// <see cref="ConsumerAdmissionAlpn"/>) that Rust does not handle. The handler owns the
    /// <see cref="Connection"/>; without a handler the connection is closed.
    /// </summary>
    public event Action<string, Connection>? NonRpcConnection;

    /// <summary>
    /// Attaches attributes (e.g. <c>aster.role</c>) to every later call from
    /// <paramref name="peerId"/>: Rust's Gate 3 checks <see cref="CapabilityRequirement"/>s
    /// against them, and handlers see them in <see cref="ServerCallContext.Attributes"/>. Null or
    /// empty clears.
    /// </summary>
    public void SetPeerAttributes(string peerId, IReadOnlyDictionary<string, string>? attributes)
    {
        ArgumentException.ThrowIfNullOrEmpty(peerId);
        if (attributes is null || attributes.Count == 0)
        {
            _peerAttributes.TryRemove(peerId, out _);
            _dispatch.SetPeerAttributes(peerId, null);
            return;
        }
        var copy = new Dictionary<string, string>(attributes, StringComparer.Ordinal);
        _peerAttributes[peerId] = copy;
        _dispatch.SetPeerAttributes(peerId, copy);
    }

    // ─── Poll loop ──────────────────────────────────────────────────────────

    private unsafe void PollLoop()
    {
        var events = (DispatchEventNative*)NativeMemory.AlignedAlloc((nuint)(PollBatch * DispatchServer.EventSize), 8);
        try
        {
            while (_running)
            {
                uint n;
                try { n = _dispatch.Poll(events, PollBatch, PollTimeoutMs); }
                catch (ObjectDisposedException) { return; }
                for (uint i = 0; i < n; i++)
                {
                    try { OnEvent(ref events[i]); }
                    catch (Exception ex) { Console.Error.WriteLine($"aster-server-poll: event failed: {ex.Message}"); }
                }
            }
        }
        finally
        {
            NativeMemory.AlignedFree(events);
        }
    }

    private void OnEvent(ref DispatchEventNative ev)
    {
        switch ((DispatchEventKind)ev.kind)
        {
            case DispatchEventKind.Call:
                OnCall(ref ev);
                break;
            case DispatchEventKind.Cancelled:
                // Rust already answered (deadline / corrupt request) or nobody is listening (peer
                // cancel): stop the handler. CancelAsync keeps its callbacks off this thread.
                if (_live.TryGetValue(ev.call_id, out var cts))
                    _ = cts.CancelAsync();
                break;
            case DispatchEventKind.ResponseWritable:
            case DispatchEventKind.ResponseClosed:
                _dispatch.HandleCapacityEvent((DispatchEventKind)ev.kind, ev.call_id);
                break;
            case DispatchEventKind.SessionClosed:
                if (_sessions.TryRemove(ev.session_key, out var state))
                    DisposeSession(state);
                Raise(SessionClosed, ev.session_key, nameof(SessionClosed));
                break;
            case DispatchEventKind.ConnectionClosed:
                _dispatch.BufferRelease(ev.buffer);
                _peers.Remove(ev.connection_id);
                Raise(ConnectionClosed, ev.connection_id, nameof(ConnectionClosed));
                break;
            case DispatchEventKind.NonRpc:
                OnNonRpc(ref ev);
                break;
            default:
                _dispatch.BufferRelease(ev.buffer);
                break;
        }
    }

    private static void Raise(Action<ulong>? handler, ulong id, string what)
    {
        try { handler?.Invoke(id); }
        catch (Exception ex) { Console.Error.WriteLine($"{what} handler failed: {ex.Message}"); }
    }

    private unsafe void OnNonRpc(ref DispatchEventNative ev)
    {
        string alpn = ev.request_len == 0 ? "" : Encoding.UTF8.GetString((byte*)ev.request_ptr, (int)ev.request_len);
        _dispatch.BufferRelease(ev.buffer);
        ulong handle = ev.connection_handle;
        var handler = NonRpcConnection;
        if (handler is not null)
        {
            try
            {
                handler(alpn, new Connection(_node.Runtime, new ConnectionHandle(handle)));
                return;
            }
            catch (Exception ex) { Console.Error.WriteLine($"NonRpcConnection handler failed: {ex.Message}"); return; }
        }
        // Owned by us and unclaimed: close and free it.
        try { new Connection(_node.Runtime, new ConnectionHandle(handle)).Close("no handler for ALPN"); }
        catch (IrohException) { /* already closed */ }
        Native.aster_connection_free(_node.Runtime.Handle, handle);
    }

    /// <summary>One call, copied out of its event on the poll thread.</summary>
    private sealed record Inbound(
        ulong CallId, ServiceDefinition Service, MethodDefinition Method, DispatchFlags Flags,
        ulong SessionKey, int SessionId, object? Session, ulong ConnectionId, long DeadlineUnixMs,
        byte[] Request, string Peer, CancellationTokenSource Cts);

    private unsafe void OnCall(ref DispatchEventNative ev)
    {
        ulong callId = ev.call_id;
        byte[] request = ev.request_len == 0 ? Array.Empty<byte>() : new ReadOnlySpan<byte>((byte*)ev.request_ptr, (int)ev.request_len).ToArray();
        if (!_peers.TryGetValue(ev.connection_id, out string? peer))
        {
            peer = ev.peer_len == 0 ? "" : Encoding.UTF8.GetString((byte*)ev.peer_ptr, (int)ev.peer_len);
            _peers[ev.connection_id] = peer;
        }
        _dispatch.BufferRelease(ev.buffer);

        ServiceDefinition? svc = ev.service_idx < _byIndex.Length ? _byIndex[ev.service_idx] : null;
        if (svc is null || ev.method_idx >= svc.OrderedMethods.Length)
        {
            // Unreachable unless Rust and this table disagree; answer off the poll thread.
            ThreadPool.UnsafeQueueUserWorkItem(static s => s.d.Abandon(s.id), (d: _dispatch, id: callId), preferLocal: false);
            return;
        }
        object? session = null;
        if (ev.session_key != 0)
        {
            // Rust decided the session may exist (graveyard, cap); here it only needs state.
            // Created on this thread, ahead of the session's SESSION_CLOSED.
            string p = peer;
            session = _sessions.GetOrAdd(ev.session_key, _ => svc.SessionFactory?.Invoke(p));
        }
        var cts = new CancellationTokenSource();
        _live[callId] = cts;
        var inbound = new Inbound(callId, svc, svc.OrderedMethods[ev.method_idx], (DispatchFlags)ev.flags,
            ev.session_key, unchecked((int)ev.session_id), session, ev.connection_id, ev.deadline_unix_ms,
            request, peer, cts);
        Interlocked.Increment(ref _inFlight);
        ThreadPool.UnsafeQueueUserWorkItem(static s => _ = s.server.DispatchAsync(s.call), (server: this, call: inbound), preferLocal: false);
    }

    private async Task DispatchAsync(Inbound inbound)
    {
        ServerCall? call = null;
        // The handler runs inside an Activity parented on the Rust server span, so Activity.Current
        // (and everything .NET instruments off it) joins the call's trace.
        Activity? activity = null;
        try
        {
            var flags = inbound.Flags;
            activity = Tracing.StartInbound(_dispatch, inbound.CallId, flags, inbound.Service.Name, inbound.Method.Name);
            IReadOnlyList<KeyValuePair<string, string>> metadata = (flags & DispatchFlags.HasMetadata) != 0
                ? _dispatch.Metadata(inbound.CallId)
                : Array.Empty<KeyValuePair<string, string>>();
            IReadOnlyDictionary<string, string> attributes = NoAttributes;
            if (_peerAttributes.TryGetValue(inbound.Peer, out var peerAttrs))
                attributes = peerAttrs;
            else if ((flags & DispatchFlags.HasAttributes) != 0)
                attributes = _dispatch.Attributes(inbound.CallId).GroupBy(kv => kv.Key).ToDictionary(g => g.Key, g => g.Last().Value);
            DateTimeOffset? deadline = inbound.DeadlineUnixMs > 0 ? DateTimeOffset.FromUnixTimeMilliseconds(inbound.DeadlineUnixMs) : null;
            var method = inbound.Method;
            var ctx = new ServerCallContext(inbound.Service.Name, method.Name, method.Pattern, inbound.Peer, inbound.ConnectionId,
                inbound.SessionId, inbound.SessionKey, inbound.Session, metadata, attributes, deadline, inbound.Cts.Token);
            call = new ServerCall(_dispatch, inbound.CallId, inbound.Request, (flags & DispatchFlags.FirstIsEnd) != 0, _codec, ctx);
            if (_interceptors.Count > 0)
            {
                // Request-side hooks see the raw first request payload (Gate 3 already ran in
                // Rust); a thrown RpcError rejects the call with its code.
                InterceptorChain.ApplyRequest(_interceptors, InterceptorContext(inbound, ctx), call.FirstRequest);
            }
            await method.InvokeAsync(call).ConfigureAwait(false);
        }
        catch (Exception e)
        {
            activity?.RecordException(e);
            try
            {
                if (call != null)
                    await call.CompleteAsync(RpcStatus.FromException(e)).ConfigureAwait(false);
                else
                    _dispatch.Abandon(inbound.CallId);
            }
            catch (Exception ex) { Console.Error.WriteLine($"aster response for call {inbound.CallId} failed: {ex.Message}"); }
        }
        finally
        {
            activity?.Dispose();
            if (call is { Completed: false }) call.Abandon();
            _live.TryRemove(inbound.CallId, out _);
            Interlocked.Decrement(ref _inFlight);
        }
    }

    private static CallContext InterceptorContext(Inbound inbound, ServerCallContext ctx)
        => new()
        {
            Service = ctx.Service,
            Method = ctx.Method,
            CallId = inbound.CallId.ToString(System.Globalization.CultureInfo.InvariantCulture),
            SessionId = ctx.SessionId == 0 ? null : ctx.SessionId.ToString(System.Globalization.CultureInfo.InvariantCulture),
            Peer = ctx.PeerId,
            Metadata = ctx.Metadata.GroupBy(kv => kv.Key).ToDictionary(g => g.Key, g => g.Last().Value),
            Attributes = new Dictionary<string, string>(ctx.Attributes),
            Deadline = ctx.Deadline?.ToUnixTimeMilliseconds() / 1000.0,
            IsStreaming = ctx.Pattern != MethodPattern.Unary,
            Pattern = ctx.Pattern switch
            {
                MethodPattern.Unary => "unary",
                MethodPattern.ServerStream => "server_stream",
                MethodPattern.ClientStream => "client_stream",
                _ => "bidi_stream",
            },
            Idempotent = inbound.Method.Idempotent,
        };

    private static void DisposeSession(object? state)
    {
        try
        {
            switch (state)
            {
                case IAsyncDisposable ad:
                    _ = ad.DisposeAsync().AsTask().ContinueWith(
                        static t => Console.Error.WriteLine($"session dispose failed: {t.Exception?.GetBaseException().Message}"),
                        TaskContinuationOptions.OnlyOnFaulted);
                    break;
                case IDisposable d:
                    d.Dispose();
                    break;
            }
        }
        catch (Exception ex) { Console.Error.WriteLine($"session dispose failed: {ex.Message}"); }
    }

    /// <summary>Stops dispatching, cancels and waits briefly for in-flight calls, then closes the node.</summary>
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        _running = false;
        _pollThread.Join(5000);
        foreach (var cts in _live.Values)
        {
            try { cts.Cancel(); }
            catch (Exception) { /* a handler callback failed; shutting down anyway */ }
        }
        for (int i = 0; i < 50 && Volatile.Read(ref _inFlight) > 0; i++)
            await Task.Delay(20).ConfigureAwait(false);
        _dispatch.Dispose();
        _health?.Dispose();
        foreach (var state in _sessions.Values) DisposeSession(state);
        _sessions.Clear();
        await _node.DisposeAsync().ConfigureAwait(false);
        _ownedGate?.Dispose();
    }

    public void Dispose() => DisposeAsync().AsTask().GetAwaiter().GetResult();
}

/// <summary>Options for <see cref="AsterServer.StartAsync"/>.</summary>
public sealed class AsterServerOptions
{
    /// <summary>Node configuration (null = defaults).</summary>
    public AsterConfig? Config { get; init; }

    /// <summary>The services to serve.</summary>
    public IList<ServiceDefinition> Services { get; init; } = new List<ServiceDefinition>();

    /// <summary>Payload codec (default: a <see cref="ForyCodec"/>; service types are registered on it).</summary>
    public ICodec? Codec { get; init; }

    /// <summary>
    /// Gate-0 policy. Default: a gate owned by the server, <see cref="TrustMode.OpenDev"/> when
    /// <see cref="AsterConfig.AllowAllConsumers"/> is set, else <see cref="TrustMode.Protected"/>
    /// (only admitted peers reach <c>aster/1</c>).
    /// </summary>
    public Gate? Gate { get; init; }

    /// <summary>
    /// Server interceptors; their <see cref="IInterceptor.OnRequest"/> runs before every handler
    /// (with the raw request payload) and may reject the call by throwing <see cref="RpcError"/>.
    /// </summary>
    public IList<IInterceptor> Interceptors { get; init; } = new List<IInterceptor>();

    /// <summary>Dispatch event ring capacity (rounded up to a power of two; 0 = 256).</summary>
    public uint RingCapacity { get; init; } = 256;

    /// <summary>
    /// Active sessions one connection may hold (spec §7.5); further session-creating calls get
    /// <c>RESOURCE_EXHAUSTED</c> from Rust. Default 1024.
    /// </summary>
    public int MaxSessionsPerConnection { get; init; } = 1024;

    /// <summary>
    /// Starts the ops listener (<c>/healthz</c>, <c>/readyz</c>, <c>/metrics</c>) on the server's node and
    /// attaches it: ready after a successful start, not ready first on dispose. Null = no listener.
    /// </summary>
    public HealthOptions? Health { get; init; }

    /// <summary>Cap on any handler's run time (null = Rust's default, 300 s; zero = no cap).</summary>
    public TimeSpan? HandlerTimeout { get; init; }
}
