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
///     Config = AsterConfig.FromEnv(),
///     Services = { service },
/// });
/// Console.WriteLine(await server.TicketAsync());
/// </code>
/// <para>
/// <b>Enrollment.</b> With enrollment configured (<see cref="AsterConfig.IsEnrollmentEnabled"/>:
/// <c>[trust]</c> and <c>[enrollment]</c>, <c>ASTER_ENROLLMENT_*</c>) the server admits only nodes
/// holding a credential from the configured root, and each peer's verified claims become the
/// roles <see cref="CapabilityRequirement"/>s are checked against, on every call, with no
/// application code (design <c>aster-enrollment.md</c> §15.4). Without it the node is open: there
/// is no "allow all" switch.
/// </para>
/// </summary>
public sealed partial class AsterServer : IAsyncDisposable, IDisposable
{
    public const string AsterAlpn = "aster/1";

    private const int PollBatch = 64;
    private const uint PollTimeoutMs = 100;

    private readonly Node _node;
    private readonly Lease.NativeLeaseSerializer? _leaseSerializer;
    private readonly Upcall.Upcalls? _leaseUpcalls;
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

    private static readonly IReadOnlyDictionary<string, string> NoAttributes = new Dictionary<string, string>();

    private AsterServer(Node node, Gate? gate, EnrollmentIssuer? issuer, Lease.NativeLeaseSerializer? leaseSerializer,
        Upcall.Upcalls? leaseUpcalls, DispatchServer dispatch, HealthServer? health, ICodec codec,
        Dictionary<string, ServiceDefinition> services, ServiceDefinition[] byIndex, IReadOnlyList<IInterceptor> interceptors)
    {
        _interceptors = interceptors;
        _node = node;
        Gate = gate;
        Issuer = issuer;
        _leaseSerializer = leaseSerializer;
        _leaseUpcalls = leaseUpcalls;
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
        // Builder overrides (Rust AsterServerBuilder::enrollment_policy / enrollment_roles);
        // enablement still comes from the configuration.
        if (options.EnrollmentPolicy is not null || options.EnrollmentRoles is not null)
            config = config with
            {
                Enrollment = config.Enrollment with
                {
                    Policy = options.EnrollmentPolicy ?? config.Enrollment.Policy,
                    Roles = options.EnrollmentRoles ?? config.Enrollment.Roles,
                },
            };
        Gate? gate = options.Gate;
        if (gate is not null && config.IsEnrollmentEnabled)
            throw new ArgumentException("enrollment decides admission; a Gate cannot be set with it", nameof(options));
        Node? node = null;
        HealthServer? health = null;
        Upcall.Upcalls? leaseUpcalls = null;
        Lease.NativeLeaseSerializer? leaseSerializer = null;
        try
        {
            node = gate is not null
                ? await Node.StartAsync(config, gate, [AsterAlpn], cancellationToken).ConfigureAwait(false)
                : await Node.StartEnrolledAsync(config, Native.EnrollmentSideServer, [AsterAlpn], cancellationToken).ConfigureAwait(false);
            EnrollmentIssuer? issuer = null;
            if (node.Enrollment is { } enrollment && enrollment.Status().Issuer is { Length: > 0 })
                issuer = new EnrollmentIssuer(enrollment);
            if (options.LeaseGrantor is { } grantor)
            {
                if (grantor.Serializer is null)
                    leaseSerializer = Lease.NativeLeaseSerializer.Memory(node.Runtime);
                else
                {
                    leaseUpcalls = Upcall.Upcalls.Create(node.Runtime);
                    leaseUpcalls.ServeLeaseSerializer(grantor.Serializer);
                    leaseSerializer = Lease.NativeLeaseSerializer.Foreign(leaseUpcalls);
                }
            }
            var byIndex = new ServiceDefinition[services.Count];
            // Explicit options win; otherwise the config (ASTER_HEALTH_*, [health]).
            var healthOptions = options.Health
                ?? (config.IsHealthEnabled ? new HealthOptions { Host = config.HealthHost, Port = config.HealthPort } : null);
            if (healthOptions is not null)
                health = await HealthServer.StartAsync(node, healthOptions).ConfigureAwait(false);
            DispatchServer dispatch;
            using (var builder = new DispatchServer.Builder(node.Runtime.Handle, node.Handle.Value))
            {
                foreach (var s in services.Values)
                    byIndex[builder.AddServiceJson(s.RegistrationJson)] = s;
                if (health is not null) builder.AttachHealth(health.Handle);
                builder.Topology(options.Topology, options.TopologyRequirement);
                builder.Option(DispatchServerOption.MaxSessionsPerConnection, (ulong)options.MaxSessionsPerConnection);
                if (options.HandlerTimeout is { } cap)
                    builder.Option(DispatchServerOption.HandlerTimeoutMs, cap <= TimeSpan.Zero ? 0 : (ulong)Math.Ceiling(cap.TotalMilliseconds));
                if (options.LeaseGrantor is { } g && leaseSerializer is not null)
                    builder.LeaseGrantor(leaseSerializer.Handle, g);
                // Gateway identity itself comes from the config (aster_server_builder_new / _start).
                if (options.AcceptOnBehalfOf is { } accept)
                    builder.Option(DispatchServerOption.AcceptOnBehalfOf, accept ? 1UL : 0UL);
                if (options.AuditOutbox is { } outbox)
                    builder.AuditOutbox(outbox.NativeHandle);
                // On an enrolled node this also attaches policy, which blocks up to
                // limits.sync_timeout_ms with replicas: keep it off the caller's thread.
                var started = builder;
                dispatch = await Task.Run(() => started.Start(options.RingCapacity), cancellationToken).ConfigureAwait(false);
            }
            return new AsterServer(node, gate, issuer, leaseSerializer, leaseUpcalls, dispatch, health, codec, services, byIndex,
                options.Interceptors.ToList());
        }
        catch
        {
            health?.Dispose();
            leaseSerializer?.Dispose();
            if (leaseUpcalls is not null) await leaseUpcalls.DisposeAsync().ConfigureAwait(false);
            node?.Dispose();
            throw;
        }
    }

    /// <summary>
    /// The Enrollment installed from <see cref="AsterServerOptions.Config"/> (null when enrollment
    /// is off). Roles and revocations need nothing from it; it is for audit, checks and
    /// revocation events.
    /// </summary>
    public Enrollment? Enrollment => _node.Enrollment;

    /// <summary>
    /// The delegated issuer the server runs when <see cref="EnrollmentConfig.IssuerDelegationFile"/>
    /// is configured (null otherwise). It already serves renewals; use
    /// <see cref="EnrollmentIssuer.Issue"/> to enroll new nodes.
    /// </summary>
    public EnrollmentIssuer? Issuer { get; }

    /// <summary>
    /// The serialization point behind <see cref="AsterServerOptions.LeaseGrantor"/> (null without
    /// one): the grantor's own rows, for snapshots and co-located holders on this node.
    /// </summary>
    public Lease.NativeLeaseSerializer? LeaseSerializer => _leaseSerializer;

    /// <summary>The server's node.</summary>
    public Node Node => _node;

    /// <summary>The ops listener started from <see cref="AsterServerOptions.Health"/>, or null.</summary>
    public HealthServer? Health => _health;

    /// <summary>
    /// The explicit Gate-0 policy (<see cref="AsterServerOptions.Gate"/>), or null: an unenrolled
    /// server is open and an enrolled one is guarded by <see cref="Enrollment"/>.
    /// </summary>
    public Gate? Gate { get; }

    /// <summary>The server's node id (64 hex).</summary>
    public string NodeId() => _node.NodeId();

    /// <summary>A ticket for this server (waits briefly for direct addresses).</summary>
    public Task<AsterTicket> TicketAsync(CancellationToken cancellationToken = default)
        => _node.TicketAsync(TimeSpan.FromSeconds(5), cancellationToken);

    private readonly object _propsLock = new();
    private Func<NodeProperties>? _nodePropertiesFn;
    private NodeProperties? _props;

    /// <summary>
    /// The server's <c>aster.ops.NodeConfig</c> properties now (Rust <c>AsterServer::node_config().properties()</c>),
    /// re-read only when the NodeConfig generation changes. Empty when NodeConfig is off.
    /// </summary>
    public NodeProperties NodeProperties()
    {
        ulong generation = _dispatch.NodeConfigGeneration;
        lock (_propsLock)
        {
            if (_props is { } cached && cached.Generation == generation) return cached;
            var json = _dispatch.NodePropertiesJson();
            try { _props = System.Text.Json.JsonSerializer.Deserialize<NodeProperties>(json, GatewayIdentity.Json) ?? Aster.NodeProperties.Empty; }
            catch (System.Text.Json.JsonException e) { throw new IrohException(Status.Internal, $"aster_server_node_properties: {e.Message}"); }
            return _props;
        }
    }

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
    /// Raised (on the poll thread) for a connection on one of the node's non-RPC ALPNs (those in
    /// <see cref="AsterConfig.Alpns"/>) that Rust does not handle. The handler owns the
    /// <see cref="Connection"/>; without a handler the connection is closed.
    /// </summary>
    public event Action<string, Connection>? NonRpcConnection;

    /// <summary>
    /// Attaches attributes (e.g. <c>aster.role</c>) to every later authenticated Iroh call from
    /// <paramref name="peerId"/>: Rust's Gate 3 checks <see cref="CapabilityRequirement"/>s
    /// against them, and handlers see them in <see cref="ServerCallContext.Attributes"/>. Null or
    /// empty clears. HTTP/WT uses the authenticator's per-call attributes instead.
    /// <para>
    /// With enrollment configured, the credential sets the maximum and runtime settings can only
    /// restrict: a peer with a live enrollment session takes its roles from its credential
    /// (<c>[enrollment.roles]</c>), so <c>aster.role</c> here throws an
    /// <see cref="IrohException"/> (<c>InvalidArgument</c>; narrow it with
    /// <see cref="RestrictPeerRoles"/>). Other keys apply, and the credential's attributes win on a
    /// clash. For a peer without a session (no enrollment, or a custom gate) these attributes,
    /// <c>aster.role</c> included, apply as given.
    /// </para>
    /// </summary>
    public void SetPeerAttributes(string peerId, IReadOnlyDictionary<string, string>? attributes)
    {
        ArgumentException.ThrowIfNullOrEmpty(peerId);
        if (attributes is null || attributes.Count == 0)
        {
            _dispatch.SetPeerAttributes(peerId, null);
            return;
        }
        var copy = new Dictionary<string, string>(attributes, StringComparer.Ordinal);
        _dispatch.SetPeerAttributes(peerId, copy);
    }

    /// <summary>
    /// Narrows an enrolled peer's roles to <paramref name="roles"/> ∩ its credential roles, from
    /// its next call (calls in flight finish). The credential sets the maximum: a role it lacks is
    /// never granted, and an empty list refuses every role-gated call. Replaces an earlier
    /// restriction, survives renewals of the same credential subject, and ends with the peer's
    /// enrollment session (revocation, eviction, expiry). Throws an <see cref="IrohException"/>
    /// (<c>NotFound</c>) when <paramref name="peerId"/> has no live enrollment session.
    /// </summary>
    public void RestrictPeerRoles(string peerId, IEnumerable<string> roles)
    {
        ArgumentException.ThrowIfNullOrEmpty(peerId);
        ArgumentNullException.ThrowIfNull(roles);
        _dispatch.RestrictPeerRoles(peerId, string.Join(",", roles));
    }

    /// <summary>Removes <paramref name="peerId"/>'s role restriction; its credential roles apply from its next call.</summary>
    public void ClearPeerRestriction(string peerId)
    {
        ArgumentException.ThrowIfNullOrEmpty(peerId);
        _dispatch.ClearPeerRestriction(peerId);
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
                for (uint i = 0; i < n && _running; i++)
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
        byte[] Request, string Peer, string TransportPeer, int WireCallId, CancellationTokenSource Cts);

    private unsafe void OnCall(ref DispatchEventNative ev)
    {
        ulong callId = ev.call_id;
        byte[] request = ev.request_len == 0 ? Array.Empty<byte>() : new ReadOnlySpan<byte>((byte*)ev.request_ptr, (int)ev.request_len).ToArray();
        // HTTP/WT has connection id zero and authenticates each call separately.
        // Caching its first principal would attribute later callers to that user.
        // A call with a distinct principal (ABI 7.5) reads its transport peer per call instead.
        var evFlags = (DispatchFlags)ev.flags;
        bool hasPrincipal = (evFlags & DispatchFlags.HasPrincipal) != 0;
        bool cachePeer = (evFlags & DispatchFlags.IrohAuthenticated) != 0 && ev.connection_id != 0 && !hasPrincipal;
        string? peer;
        if (!cachePeer || !_peers.TryGetValue(ev.connection_id, out peer))
        {
            peer = ev.peer_len == 0 ? "" : Encoding.UTF8.GetString((byte*)ev.peer_ptr, (int)ev.peer_len);
            if (cachePeer) _peers[ev.connection_id] = peer;
        }
        _dispatch.BufferRelease(ev.buffer);
        // peer_ptr is the principal; with HAS_PRINCIPAL the transport peer comes from
        // aster_dispatch_identity while the call is live.
        string? transportPeer = ResolveTransportPeer(hasPrincipal, peer, hasPrincipal ? _dispatch.TransportPeer(callId) : null);
        if (transportPeer is null)
        {
            // NOT_FOUND: the call left Rust's table (cancelled or past its deadline) before this
            // thread read it, and Rust already answered it. Nothing runs for it.
            return;
        }

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
            request, peer, transportPeer, ev.wire_call_id, cts);
        Interlocked.Increment(ref _inFlight);
        ThreadPool.UnsafeQueueUserWorkItem(static s => _ = s.server.DispatchAsync(s.call), (server: this, call: inbound), preferLocal: false);
    }

    /// <summary>
    /// A call's transport peer: <paramref name=peer/> (the event's peer) without HAS_PRINCIPAL, else
    /// <paramref name=identityPeer/> from <c>aster_dispatch_identity</c>. Null when that read found the
    /// call over (<c>NOT_FOUND</c>): the caller drops the call rather than report the principal (the user,
    /// on a forwarded call) as the transport peer and the actor.
    /// </summary>
    internal static string? ResolveTransportPeer(bool hasPrincipal, string peer, string? identityPeer)
        => hasPrincipal ? identityPeer : peer;

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
            // Use the same per-call snapshot Rust used for Gate 3. Enrollment
            // attributes must not override a foreign authenticator's outcome.
            if ((flags & DispatchFlags.HasAttributes) != 0)
                attributes = _dispatch.Attributes(inbound.CallId).GroupBy(kv => kv.Key).ToDictionary(g => g.Key, g => g.Last().Value);
            DateTimeOffset? deadline = inbound.DeadlineUnixMs > 0 ? DateTimeOffset.FromUnixTimeMilliseconds(inbound.DeadlineUnixMs) : null;
            var method = inbound.Method;
            // A gateway's on-behalf-of call (ABI 7.7): the gateway is the transport peer, the user the principal.
            string? actor = null;
            ForwardedIdentity? onBehalfOf = null;
            if ((flags & DispatchFlags.OnBehalfOf) != 0)
            {
                actor = inbound.TransportPeer;
                onBehalfOf = _dispatch.OnBehalfOf(inbound.CallId);
            }
            var ctx = new ServerCallContext(inbound.Service.Name, method.Name, method.Pattern, inbound.Peer, inbound.ConnectionId,
                inbound.SessionId, inbound.SessionKey, inbound.Session, metadata, attributes, deadline, inbound.Cts.Token,
                (flags & DispatchFlags.IrohAuthenticated) != 0, inbound.TransportPeer, inbound.CallId, inbound.WireCallId,
                actor, onBehalfOf, _nodePropertiesFn ??= NodeProperties);
            // Path 2 of the handler-context design: the async-local, scoped to this async method.
            ServerCallContext.SetCurrent(ctx);
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
        await _httpLifecycleGate.WaitAsync().ConfigureAwait(false);
        try
        {
            if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
            _running = false;
            if (Thread.CurrentThread != _pollThread) _pollThread.Join(5000);
            foreach (var cts in _live.Values)
            {
                try { cts.Cancel(); }
                catch (Exception) { /* a handler callback failed; shutting down anyway */ }
            }
            for (int i = 0; i < 50 && Volatile.Read(ref _inFlight) > 0; i++)
                await Task.Delay(20).ConfigureAwait(false);
            await Task.Run(_dispatch.Dispose).ConfigureAwait(false);
            _health?.Dispose();
            foreach (var state in _sessions.Values) DisposeSession(state);
            _sessions.Clear();
            _leaseSerializer?.Dispose();
            if (_leaseUpcalls is not null) await _leaseUpcalls.DisposeAsync().ConfigureAwait(false);
            await _node.DisposeAsync().ConfigureAwait(false);
        }
        finally { _httpLifecycleGate.Release(); }
    }

    public void Dispose() => DisposeAsync().AsTask().GetAwaiter().GetResult();
}

/// <summary>Options for <see cref="AsterServer.StartAsync"/>.</summary>
public sealed class AsterServerOptions
{
    /// <summary>Explicitly expose aster.net.Topology. Requires Config.EnableMonitoring. Default false.</summary>
    public bool Topology { get; init; }
    /// <summary>Optional native Gate-3 requirement for the topology RPC, without host authorization logic.</summary>
    public CapabilityRequirement? TopologyRequirement { get; init; }

    /// <summary>
    /// Node configuration (null = defaults). With enrollment configured
    /// (<see cref="AsterConfig.IsEnrollmentEnabled"/>) the server admits only nodes holding a
    /// credential from the configured root, and each peer's verified claims become the roles
    /// <see cref="CapabilityRequirement"/>s are checked against, on every call. Without it the
    /// node is open. A missing or malformed enrollment setting fails the start, naming it.
    /// </summary>
    public AsterConfig? Config { get; init; }

    /// <summary>
    /// Overrides <see cref="EnrollmentConfig.Policy"/> (Rust <c>AsterServerBuilder::enrollment_policy</c>).
    /// Enablement still comes from <see cref="Config"/>.
    /// </summary>
    public EnrollmentPolicy? EnrollmentPolicy { get; init; }

    /// <summary>
    /// Overrides <see cref="EnrollmentConfig.Roles"/> (Rust <c>AsterServerBuilder::enrollment_roles</c>).
    /// Enablement still comes from <see cref="Config"/>.
    /// </summary>
    public RoleMapping? EnrollmentRoles { get; init; }

    /// <summary>
    /// Serves <c>aster.lease.Grantor</c> (Rust <c>aster::rpc::lease::LeaseGrantor</c>): a designated
    /// serialization point other nodes acquire, renew and release leases through
    /// (<see cref="AsterClient.LeaseGrantorAsync(NodeAddrInfo, CancellationToken)"/>). Null = none.
    /// </summary>
    public LeaseGrantorOptions? LeaseGrantor { get; init; }

    /// <summary>The services to serve.</summary>
    public IList<ServiceDefinition> Services { get; init; } = new List<ServiceDefinition>();

    /// <summary>Payload codec (default: a <see cref="ForyCodec"/>; service types are registered on it).</summary>
    public ICodec? Codec { get; init; }

    /// <summary>
    /// An explicit Gate-0 policy (an escape hatch; Rust's <c>Gate0</c>), owned by the caller.
    /// Default: none, so an unenrolled server is open and an enrolled one is guarded by
    /// Enrollment. Setting it with enrollment configured is an error.
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
    /// attaches it: ready after a successful start, not ready first on dispose. Null = the listener the
    /// config asks for (<see cref="AsterConfig.HealthPort"/> / <see cref="AsterConfig.HealthEnabled"/>), if any.
    /// </summary>
    public HealthOptions? Health { get; init; }

    /// <summary>Cap on any handler's run time (null = Rust's default, 300 s; zero = no cap).</summary>
    public TimeSpan? HandlerTimeout { get; init; }

    /// <summary>
    /// Overrides <see cref="IdentityConfig.AcceptOnBehalfOf"/> (Rust <c>AsterServerBuilder::accept_on_behalf_of</c>):
    /// accept calls an edge holding <see cref="GatewayIdentity.GatewayRole"/> makes on behalf of a user. Off, a
    /// call carrying the entry is refused with <c>PERMISSION_DENIED</c>. <c>aster.ops.NodeConfig</c> can still
    /// override it remotely unless <see cref="RemoteConfig.Lock"/> holds <c>identity</c>. Null = the config.
    /// </summary>
    public bool? AcceptOnBehalfOf { get; init; }

    /// <summary>
    /// Durably records every <c>aster.ops.NodeConfig</c> set and clear (Rust <c>AsterServerBuilder::audit_outbox</c>);
    /// a change the outbox does not accept is refused. The caller keeps owning it; dispose it after the server.
    /// Null = none.
    /// </summary>
    public Audit.AuditOutbox? AuditOutbox { get; init; }
}
