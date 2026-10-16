using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Runtime.ExceptionServices;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Codec;
using Aster.Interceptors;
using Aster.Observability;
using Aster.Rpc;

namespace Aster;

/// <summary>Per-call options.</summary>
public sealed record CallOptions
{
    /// <summary>
    /// Relative deadline. Enforced by Rust on the client (<c>DEADLINE_EXCEEDED</c>) and sent
    /// rounded <b>up</b> to whole seconds (max 32767 s).
    /// </summary>
    public TimeSpan? Deadline { get; init; }

    /// <summary>
    /// True if the method is safe to retry (<see cref="ServiceDefinition"/>'s <c>idempotent</c>); a
    /// <see cref="RetryInterceptor"/> retries only idempotent calls.
    /// </summary>
    public bool Idempotent { get; init; }

    /// <summary>Request metadata (stream header).</summary>
    public IReadOnlyList<KeyValuePair<string, string>>? Metadata { get; init; }

    /// <summary>
    /// 0 = shared service; non-zero = a session of a session-scoped service (take ids from
    /// <see cref="RpcConnection.NextSessionId"/>). Unary calls of one session share one pooled stream.
    /// </summary>
    public int SessionId { get; init; }

    /// <summary>Service version (<c>StreamHeader.version</c>).</summary>
    public int Version { get; init; } = 1;

    /// <summary>
    /// Makes the call on behalf of a user (Rust <c>CallOptions::on_behalf_of</c>): the JWT (a provider's token,
    /// forwarded unchanged, or one from <see cref="AsterClient.MintUserToken"/>) goes in the
    /// <see cref="GatewayIdentity.OnBehalfOfMetadata"/> entry, replacing any in <see cref="Metadata"/>. The
    /// server must accept it and this node must hold <see cref="GatewayIdentity.GatewayRole"/>. A client running
    /// <c>[identity.publisher]</c> notices the token's key id, so a provider's key rotation is published at once.
    /// </summary>
    public string? OnBehalfOf { get; init; }

    /// <summary>The metadata sent: <see cref="Metadata"/> with <see cref="OnBehalfOf"/> replacing any earlier entry.</summary>
    internal IReadOnlyList<KeyValuePair<string, string>>? EffectiveMetadata()
    {
        if (string.IsNullOrEmpty(OnBehalfOf)) return Metadata;
        var md = new List<KeyValuePair<string, string>>();
        if (Metadata is { } given)
            foreach (var kv in given)
                if (!string.Equals(kv.Key, GatewayIdentity.OnBehalfOfMetadata, StringComparison.OrdinalIgnoreCase)) md.Add(kv);
        md.Add(new(GatewayIdentity.OnBehalfOfMetadata, OnBehalfOf));
        return md;
    }

    internal const uint MaxDeadlineMs = 32_767_000;

    internal uint DeadlineMs => Deadline is { } d && d > TimeSpan.Zero
        ? (uint)Math.Min(MaxDeadlineMs, Math.Ceiling(d.TotalMilliseconds))
        : 0u;
}

/// <summary>
/// An Aster RPC client: a node plus a codec and a cache of <see cref="RpcConnection"/>s.
/// <code>
/// await using var client = await AsterClient.StartAsync(new AsterConfig { RelayMode = "disabled" });
/// client.Fory.Register&lt;EchoRequest&gt;("interop/EchoRequest");
/// var conn = await client.ConnectAsync(AsterTicket.Parse(ticket));
/// EchoReply reply = await conn.UnaryAsync&lt;EchoRequest, EchoReply&gt;("InteropEcho", "echo", request);
/// </code>
/// <para>
/// <b>Enrollment.</b> With enrollment configured (<see cref="AsterConfig.IsEnrollmentEnabled"/>)
/// the client presents its credential: before its first connection to a server it enrolls over
/// the reciprocal handshake (the server verifies this node, this node verifies the server),
/// synchronizes the fleet namespace the server offers, and renews its credential from that
/// server when the server signed it (or from <see cref="EnrollmentConfig.RenewFrom"/>). Its roles
/// on the server are the ones in its credential (design <c>aster-enrollment.md</c> §15.7).
/// Without enrollment calls go straight to <c>aster/1</c>.
/// </para>
/// <para>
/// <b>Interceptors.</b> Pass client interceptors (<see cref="DeadlineInterceptor"/>,
/// <see cref="RetryInterceptor"/>, <see cref="CircuitBreakerInterceptor"/>, your own
/// <see cref="IInterceptor"/>s) to <see cref="StartAsync"/> / <see cref="Create"/>; every
/// connection of the client applies them to all four call patterns (the semantics are those of
/// Python's client: retry and the breaker wrap unary and client-streaming calls, the hooks see
/// every message of every pattern). A circuit breaker is stateful and shared by every call of
/// the client: to isolate peers or services use <see cref="RpcConnection.WithInterceptors"/>
/// for one breaker per target.
/// </para>
/// </summary>
public sealed class AsterClient : IAsyncDisposable
{
    private readonly Node _node;
    private readonly bool _ownsNode;
    private readonly ConcurrentDictionary<string, Task<RpcConnection>> _connections = new();

    private readonly ClientInterceptors _interceptors;

    private AsterClient(Node node, ICodec codec, bool ownsNode, IEnumerable<IInterceptor>? interceptors)
    {
        _node = node;
        Codec = codec;
        _ownsNode = ownsNode;
        _interceptors = interceptors is null ? ClientInterceptors.None : new ClientInterceptors(interceptors);
    }

    /// <summary>
    /// Starts a client node (adds the <c>aster/1</c> ALPN to <paramref name="config"/>) and, when
    /// the config enables enrollment, installs Enrollment from it: a missing or malformed setting
    /// fails here, naming the setting (a credential is required; issuer settings are refused).
    /// </summary>
    public static Task<AsterClient> StartAsync(AsterConfig? config = null, ICodec? codec = null,
        IEnumerable<IInterceptor>? interceptors = null, CancellationToken cancellationToken = default)
        => StartAsync(new AsterClientOptions { Config = config, Codec = codec, Interceptors = interceptors }, cancellationToken);

    /// <summary>Starts a client from <paramref name="options"/> (see <see cref="StartAsync(AsterConfig?, ICodec?, IEnumerable{IInterceptor}?, CancellationToken)"/>).</summary>
    public static async Task<AsterClient> StartAsync(AsterClientOptions options, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(options);
        AsterObservability.EnsureStarted();
        var config = options.Config ?? new AsterConfig();
        if (options.EnrollmentPolicy is { } policy)
            config = config with { Enrollment = config.Enrollment with { Policy = policy } };
        var node = await Node.StartEnrolledAsync(config, Native.EnrollmentSideClient, [AsterServer.AsterAlpn], cancellationToken)
            .ConfigureAwait(false);
        return new AsterClient(node, options.Codec ?? new ForyCodec(), ownsNode: true, options.Interceptors);
    }

    /// <summary>A client on an existing node (which the caller keeps owning).</summary>
    public static AsterClient Create(Node node, ICodec? codec = null, IEnumerable<IInterceptor>? interceptors = null)
    {
        AsterObservability.EnsureStarted();
        return new(node, codec ?? new ForyCodec(), ownsNode: false, interceptors);
    }

    public Node Node => _node;

    /// <summary>The client interceptors, in the order they run.</summary>
    public IReadOnlyList<IInterceptor> Interceptors => _interceptors.All;

    public ICodec Codec { get; }

    /// <summary>The codec as a <see cref="ForyCodec"/>, for type registration.</summary>
    public ForyCodec Fory => Codec as ForyCodec ?? throw new InvalidOperationException("the client codec is not a ForyCodec");

    /// <summary>Registers the request/response types of <paramref name="service"/> with the codec.</summary>
    public AsterClient RegisterTypes(ServiceDefinition service)
    {
        if (Codec is ForyCodec fory)
            foreach (var m in service.Methods.Values) m.RegisterTypes(fory);
        return this;
    }

    /// <summary>A (cached) RPC connection to <paramref name="addr"/>.</summary>
    public Task<RpcConnection> ConnectAsync(NodeAddrInfo addr, CancellationToken cancellationToken = default)
    {
        var task = _connections.GetOrAdd(addr.EndpointId, _ => ConnectCore(addr, cancellationToken));
        if (task.IsFaulted || task.IsCanceled)
        {
            _connections.TryRemove(new KeyValuePair<string, Task<RpcConnection>>(addr.EndpointId, task));
            task = _connections.GetOrAdd(addr.EndpointId, _ => ConnectCore(addr, cancellationToken));
        }
        return task;
    }

    public Task<RpcConnection> ConnectAsync(AsterTicket ticket, CancellationToken cancellationToken = default)
        => ConnectAsync(ticket.ToNodeAddr(), cancellationToken);

    /// <summary>
    /// Drops the cached connection to <paramref name="addr"/> (for one the server closed, e.g.
    /// after a revocation) and opens a new one, enrolling again first when enrollment is configured.
    /// </summary>
    public async Task<RpcConnection> ReconnectAsync(NodeAddrInfo addr, CancellationToken cancellationToken = default)
    {
        if (_connections.TryRemove(addr.EndpointId, out var old) && old.IsCompletedSuccessfully)
            await old.Result.DisposeAsync().ConfigureAwait(false);
        Enrollment?.Forget(addr.EndpointId);
        return await ConnectAsync(addr, cancellationToken).ConfigureAwait(false);
    }

    private async Task<RpcConnection> ConnectCore(NodeAddrInfo addr, CancellationToken ct)
    {
        // Enroll before the first aster/1 connection to each server (design §15.7).
        if (Enrollment is { } enrollment)
            await enrollment.EnrollAsync(addr, force: false, ct).ConfigureAwait(false);
        var conn = await _node.ConnectAsync(addr, AsterServer.AsterAlpn, ct).ConfigureAwait(false);
        return new RpcConnection(conn, Codec, _interceptors, enrollment: Enrollment);
    }

    /// <summary>The Enrollment installed from the config (null when enrollment is off).</summary>
    public Enrollment? Enrollment => _node.Enrollment;

    /// <summary>
    /// Mints a short-lived token for user <paramref name="sub"/> with <paramref name="roles"/>, signed in by
    /// <paramref name="amr"/> (at least one, e.g. <see cref="GatewayIdentity.AmrWebAuthn"/>), valid for
    /// <paramref name="ttl"/> (at most 5 minutes), signed with this node's key (Rust
    /// <c>AsterClient::user_token_issuer</c> + <c>Issuer::mint_user_token</c>). The roles must be within the issuer
    /// delegation (<see cref="GatewayIdentity.IssuableRolesAttribute"/>) of the credential held now; anything else
    /// is an <see cref="IrohException"/> (<c>InvalidArgument</c>). Attach it with <see cref="CallOptions.OnBehalfOf"/>.
    /// </summary>
    public string MintUserToken(string sub, IEnumerable<string> roles, IEnumerable<string> amr, TimeSpan ttl)
        => (Enrollment ?? throw new IrohException(Status.InvalidArgument, "minting users' tokens needs [enrollment]"))
            .MintUserToken(sub, roles, amr, ttl);

    /// <summary>Whether this client renews its credential (Rust <c>AsterClient::is_renewing</c>).</summary>
    public bool IsRenewing
    {
        get
        {
            if (Enrollment is not { } e) return false;
            try { return e.Status().Renewing; }
            catch (Exception ex) when (ex is IrohException or ObjectDisposedException) { return false; }
        }
    }

    /// <summary>
    /// A lease serializer backed by the <c>aster.lease.Grantor</c> the server at
    /// <paramref name="addr"/> serves (Rust <c>GrantorSerializer</c>): acquire, snapshot and revoke
    /// are calls to it, and acquired leases renew through it in the background. Acquire with
    /// holder = this client's node id (<see cref="Node.NodeId"/>): the grantor binds this client's
    /// calls to it. Enrolls with the server first when enrollment is configured. Dispose the
    /// serializer when done.
    /// </summary>
    public async Task<Lease.NativeLeaseSerializer> LeaseGrantorAsync(NodeAddrInfo addr, CancellationToken cancellationToken = default)
    {
        var rc = await ConnectAsync(addr, cancellationToken).ConfigureAwait(false);
        Native.Check(Native.aster_lease_serializer_grantor_new(_node.Runtime.Handle, _node.Handle.Value, rc.Connection.Handle.Value,
            out ulong serializer), "aster_lease_serializer_grantor_new");
        return new Lease.NativeLeaseSerializer(_node.Runtime, serializer);
    }

    /// <inheritdoc cref="LeaseGrantorAsync(NodeAddrInfo, CancellationToken)"/>
    public Task<Lease.NativeLeaseSerializer> LeaseGrantorAsync(AsterTicket ticket, CancellationToken cancellationToken = default)
        => LeaseGrantorAsync(ticket.ToNodeAddr(), cancellationToken);

    public async ValueTask DisposeAsync()
    {
        foreach (var t in _connections.Values)
        {
            if (t.IsCompletedSuccessfully)
                await t.Result.DisposeAsync().ConfigureAwait(false);
        }
        _connections.Clear();
        if (_ownsNode)
            await _node.DisposeAsync().ConfigureAwait(false);
    }
}

/// <summary>Options for <see cref="AsterClient.StartAsync(AsterClientOptions, CancellationToken)"/>.</summary>
public sealed class AsterClientOptions
{
    /// <summary>Node configuration (null = defaults), including <c>[trust]</c> and <c>[enrollment]</c>.</summary>
    public AsterConfig? Config { get; init; }

    /// <summary>Payload codec (default: a <see cref="ForyCodec"/>).</summary>
    public ICodec? Codec { get; init; }

    /// <summary>Client interceptors, in the order they run.</summary>
    public IEnumerable<IInterceptor>? Interceptors { get; init; }

    /// <summary>
    /// Judges servers' enrollment evidence with this policy instead of
    /// <see cref="EnrollmentConfig.Policy"/> (Rust <c>AsterClientBuilder::enrollment_policy</c>).
    /// </summary>
    public EnrollmentPolicy? EnrollmentPolicy { get; init; }
}

/// <summary>
/// RPC calls over one QUIC connection through the payload-level C ABI client (3.5,
/// <c>aster_rpc_*</c> / <c>aster_call_*_payload</c>) on Rust's pooled <c>RpcConnection</c>: Rust
/// builds the stream header, frames, compresses, enforces the deadline and decodes trailers;
/// .NET passes options and payloads. Unary calls use the pooled streams, with a native cancel
/// token when the <see cref="CancellationToken"/> can be cancelled (ABI 4.5,
/// <c>aster_rpc_unary_cancellable</c>); every streaming call gets its own stream
/// (<c>aster_call_cancel</c>). Either way cancelling the token cancels the native call and the
/// server sees the cancel. Blocking native calls run on the thread pool.
/// </summary>
public sealed class RpcConnection : IAsyncDisposable
{
    private const uint RecvWaitMs = 250;
    private const byte PatternServerStream = 1;
    private const byte PatternClientStream = 2;
    private const byte PatternBidiStream = 3;

    private readonly Connection _connection;
    private readonly ICodec _codec;
    private readonly ClientInterceptors _interceptors;
    private readonly bool _ownsConnection;
    private readonly Enrollment? _enrollment;

    internal RpcConnection(Connection connection, ICodec codec, ClientInterceptors interceptors, bool ownsConnection = true,
        Enrollment? enrollment = null)
    {
        _connection = connection;
        _codec = codec;
        _interceptors = interceptors;
        _ownsConnection = ownsConnection;
        _enrollment = enrollment;
    }

    public Connection Connection => _connection;

    /// <summary>The interceptors applied to this connection's calls, in the order they run.</summary>
    public IReadOnlyList<IInterceptor> Interceptors => _interceptors.All;

    /// <summary>
    /// A view of this connection with its own interceptors (replacing, not adding to, the
    /// client's): it shares the QUIC connection, so disposing the view does nothing. Use it for a
    /// circuit breaker (stateful) per service or per peer.
    /// </summary>
    public RpcConnection WithInterceptors(IEnumerable<IInterceptor> interceptors)
        => new(_connection, _codec, new ClientInterceptors(interceptors), ownsConnection: false, _enrollment);

    /// <summary>The call's options; a forwarded token is handed to this client's issuer-key publisher, if it runs one.</summary>
    private CallSpec Spec(string service, string method, CallOptions? options)
    {
        if (options?.OnBehalfOf is { Length: > 0 } token && _enrollment is { } e)
        {
            // The publisher ignores tokens of issuers it does not publish; NOT_FOUND = no publisher.
            try { e.NoticeToken(token); }
            catch (Exception ex) when (ex is IrohException or ObjectDisposedException) { }
        }
        return new CallSpec(service, method, options);
    }

    private string? Peer()
    {
        try { return _connection.RemoteId(); }
        catch (IrohException) { return null; }
    }

    private ulong Rt => _connection.Runtime.Handle;
    private ulong Conn => _connection.Handle.Value;

    /// <summary>
    /// A fresh session id for a session-scoped service (<see cref="CallOptions.SessionId"/>), from
    /// the connection's shared counter (unique across every client on the connection). The server
    /// creates the session on the first call carrying it.
    /// </summary>
    public int NextSessionId()
    {
        int r = Native.aster_connection_next_session_id(Rt, Conn, out uint id);
        if (r != 0) throw CallError(r, "aster_connection_next_session_id");
        return unchecked((int)id);
    }

    /// <summary>Encoded per-call options (<c>aster_call_options_t</c> fields).</summary>
    private readonly struct CallSpec(string service, string method, CallOptions? options)
    {
        public readonly byte[] Service = Encoding.UTF8.GetBytes(service);
        public readonly byte[] Method = Encoding.UTF8.GetBytes(method);
        // The current Activity (any source) is the parent of the call: traceparent / baggage go out as
        // call metadata and the Rust client span nests under it (design §5.4).
        public readonly byte[] Metadata = Tracing.Inject(options?.EffectiveMetadata()) is { Count: > 0 } md ? KvList.Encode(md) : Array.Empty<byte>();
        public readonly uint SessionId = unchecked((uint)(options?.SessionId ?? 0));
        public readonly uint DeadlineMs = options?.DeadlineMs ?? 0;
        public readonly int Version = options?.Version ?? 1;

        public unsafe CallOptionsNative Native(byte* service, byte* method, byte* metadata) => new()
        {
            struct_size = (uint)sizeof(CallOptionsNative),
            version = Version,
            service = Bytes.FromPointer(service, Service.Length),
            method = Bytes.FromPointer(method, Method.Length),
            metadata = Bytes.FromPointer(metadata, Metadata.Length),
            session_id = SessionId,
            deadline_ms = DeadlineMs,
        };
    }

    /// <summary>A unary call.</summary>
    public Task<TResp> UnaryAsync<TReq, TResp>(string service, string method, TReq request,
        CallOptions? options = null, CancellationToken cancellationToken = default)
    {
        if (_interceptors.IsEmpty)
            return UnaryCore<TReq, TResp>(service, method, request, options, cancellationToken);
        var ctx = _interceptors.NewContext(service, method, options, "unary", Peer());
        return _interceptors.RunAsync(ctx, request, options, hookRequest: true,
            (r, o) => UnaryCore<TReq, TResp>(service, method, r, o, cancellationToken), cancellationToken);
    }

    private async Task<TResp> UnaryCore<TReq, TResp>(string service, string method, TReq request,
        CallOptions? options, CancellationToken cancellationToken)
    {
        byte[] req = _codec.Encode(request);
        var spec = Spec(service, method, options);
        cancellationToken.ThrowIfCancellationRequested();
        // Pooled fast path: one blocking native call (header + request in one write). A
        // cancellable token gets a native cancel token (ABI 4.5), so cancelling it aborts the call
        // like dropping the Rust future: FLAG_CANCEL + reset, and the server sees the cancel.
        var (code, response, status) = cancellationToken.CanBeCanceled
            ? await Task.Run(() => UnaryCancellable(spec, req, cancellationToken), CancellationToken.None).ConfigureAwait(false)
            : await Task.Run(() => UnaryNative(spec, req, 0), CancellationToken.None).ConfigureAwait(false);
        if (code != 0)
        {
            cancellationToken.ThrowIfCancellationRequested();
            throw StatusError(code, status);
        }
        if (response.Length == 0)
            throw new RpcError(StatusCode.Internal, "OK trailer with no response frame");
        return _codec.Decode<TResp>(response);
    }

    private (int Code, byte[] Payload, byte[] Status) UnaryCancellable(CallSpec spec, byte[] request, CancellationToken ct)
    {
        int r = Native.aster_cancel_token_new(Rt, out ulong token);
        if (r != 0) throw CallError(r, "aster_cancel_token_new");
        ulong rt = Rt;
        // Runs at once if ct is already cancelled (the call then returns CANCELLED without
        // sending); a cancel after the call returned is a native no-op (NOT_FOUND).
        using var reg = ct.UnsafeRegister(_ => Native.aster_cancel_token_cancel(rt, token), null);
        return UnaryNative(spec, request, token);
    }

    private unsafe (int Code, byte[] Payload, byte[] Status) UnaryNative(CallSpec spec, byte[] request, ulong token)
    {
        byte* respPtr = null, statusPtr = null;
        uint respLen = 0, statusLen = 0;
        ulong respBuf = 0, statusBuf = 0;
        int code = 0;
        int r;
        fixed (byte* sp = spec.Service, mp = spec.Method, kp = spec.Metadata, rp = request)
        {
            CallOptionsNative o = spec.Native(sp, mp, kp);
            r = token == 0
                ? Native.aster_rpc_unary(Rt, Conn, &o, request.Length == 0 ? null : rp, (uint)request.Length,
                    &respPtr, &respLen, &respBuf, &code, &statusPtr, &statusLen, &statusBuf)
                : Native.aster_rpc_unary_cancellable(Rt, Conn, &o, token, request.Length == 0 ? null : rp, (uint)request.Length,
                    &respPtr, &respLen, &respBuf, &code, &statusPtr, &statusLen, &statusBuf);
        }
        if (r != 0) throw CallError(r, token == 0 ? "aster_rpc_unary" : "aster_rpc_unary_cancellable");
        try
        {
            return (code,
                respLen == 0 ? Array.Empty<byte>() : new ReadOnlySpan<byte>(respPtr, (int)respLen).ToArray(),
                statusLen == 0 ? Array.Empty<byte>() : new ReadOnlySpan<byte>(statusPtr, (int)statusLen).ToArray());
        }
        finally
        {
            if (respBuf != 0) Native.aster_call_buffer_release(Rt, respBuf);
            if (statusBuf != 0) Native.aster_call_buffer_release(Rt, statusBuf);
        }
    }

    /// <summary>A server-streaming call: one request, a stream of responses.</summary>
    public IAsyncEnumerable<TResp> ServerStreamAsync<TReq, TResp>(string service, string method, TReq request,
        CallOptions? options = null, CancellationToken cancellationToken = default)
    {
        if (_interceptors.IsEmpty)
            return ServerStreamCore<TReq, TResp>(service, method, request, options, cancellationToken);
        var ctx = _interceptors.NewContext(service, method, options, "server_stream", Peer());
        return _interceptors.ServerStreamAsync<TReq, TResp>(ctx, request, options,
            (r, o, ct) => ServerStreamCore<TReq, TResp>(service, method, r, o, ct), cancellationToken);
    }

    private async IAsyncEnumerable<TResp> ServerStreamCore<TReq, TResp>(string service, string method, TReq request,
        CallOptions? options, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        await using var call = await PayloadCall.OpenAsync(this, PatternServerStream, Spec(service, method, options), cancellationToken)
            .ConfigureAwait(false);
        call.Send(_codec.Encode(request), end: true);
        await foreach (var payload in call.ReadResponsesAsync(cancellationToken).ConfigureAwait(false))
            yield return _codec.Decode<TResp>(payload);
    }

    /// <summary>A client-streaming call: requests are produced concurrently with the response.</summary>
    /// <remarks>An early native response ends the RPC without waiting for a source that ignores
    /// cancellation. Such a source must release its own pending MoveNext operation; disposal is
    /// deferred until that operation ends, and arbitrary user cleanup cannot delay the RPC.
    /// With retry interceptors, requests must be replayable: each enumeration must reproduce
    /// the same sequence independently; a consumed one-shot source cannot be retried safely.</remarks>
    public Task<TResp> ClientStreamAsync<TReq, TResp>(string service, string method, IAsyncEnumerable<TReq> requests,
        CallOptions? options = null, CancellationToken cancellationToken = default)
    {
        if (_interceptors.IsEmpty)
            return ClientStreamCore<TReq, TResp>(service, method, requests, options, cancellationToken);
        var ctx = _interceptors.NewContext(service, method, options, "client_stream", Peer());
        return _interceptors.RunAsync<object?, TResp>(ctx, null, options, hookRequest: false,
            (_, o) => ClientStreamCore<TReq, TResp>(service, method, _interceptors.HookRequests(requests, ctx), o, cancellationToken),
            cancellationToken);
    }

    private async Task<TResp> ClientStreamCore<TReq, TResp>(string service, string method, IAsyncEnumerable<TReq> requests,
        CallOptions? options, CancellationToken cancellationToken)
    {
        await using var call = await PayloadCall.OpenAsync(this, PatternClientStream, Spec(service, method, options), cancellationToken)
            .ConfigureAwait(false);
        using var sendCts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        var sending = Task.Run(() => call.SendAllAsync(requests, _codec, sendCts.Token), CancellationToken.None);
        var reading = call.ReadSingleAsync(cancellationToken);
        ObserveDetached(sending);
        ObserveDetached(reading);
        byte[] response;
        try
        {
            if (await Task.WhenAny(sending, reading).ConfigureAwait(false) == sending)
            {
                try { await sending.ConfigureAwait(false); }
                catch
                {
                    // Producer failure cancels natively; drain the owned reader before release.
                    try { await reading.ConfigureAwait(false); } catch { }
                    throw;
                }
            }
            response = await reading.ConfigureAwait(false);
        }
        finally { sendCts.Cancel(); }
        if (response.Length == 0)
            throw new RpcError(StatusCode.Internal, "OK trailer with no response frame");
        return _codec.Decode<TResp>(response);
    }

    /// <inheritdoc cref="ClientStreamAsync{TReq,TResp}(string,string,IAsyncEnumerable{TReq},CallOptions?,CancellationToken)"/>
    public Task<TResp> ClientStreamAsync<TReq, TResp>(string service, string method, IEnumerable<TReq> requests,
        CallOptions? options = null, CancellationToken cancellationToken = default)
        => ClientStreamAsync<TReq, TResp>(service, method, requests.ToAsyncEnumerable(), options, cancellationToken);

    /// <summary>A bidi-streaming call: requests are sent concurrently with reading responses.</summary>
    /// <remarks>Sources should observe their cancellation token. An uncooperative pending
    /// MoveNext cannot be forcibly stopped; source disposal follows its eventual completion.</remarks>
    public IAsyncEnumerable<TResp> BidiStreamAsync<TReq, TResp>(string service, string method, IAsyncEnumerable<TReq> requests,
        CallOptions? options = null, CancellationToken cancellationToken = default)
    {
        if (_interceptors.IsEmpty)
            return BidiStreamCore<TReq, TResp>(service, method, requests, options, cancellationToken);
        return BidiStreamIntercepted<TReq, TResp>(service, method, requests, options, cancellationToken);
    }

    private async IAsyncEnumerable<TResp> BidiStreamIntercepted<TReq, TResp>(string service, string method, IAsyncEnumerable<TReq> requests,
        CallOptions? options, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        var ctx = _interceptors.NewContext(service, method, options, "bidi_stream", Peer());
        var o = _interceptors.StartStreaming(ctx, options);
        var source = BidiStreamCore<TReq, TResp>(service, method, _interceptors.HookRequests(requests, ctx), o, cancellationToken);
        await foreach (var item in _interceptors.Guard(source, ctx, cancellationToken).ConfigureAwait(false))
            yield return item;
    }

    private async IAsyncEnumerable<TResp> BidiStreamCore<TReq, TResp>(string service, string method, IAsyncEnumerable<TReq> requests,
        CallOptions? options, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        await using var call = await PayloadCall.OpenAsync(this, PatternBidiStream, Spec(service, method, options), cancellationToken)
            .ConfigureAwait(false);
        using var sendCts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        var sending = Task.Run(() => call.SendAllAsync(requests, _codec, sendCts.Token), CancellationToken.None);
        try
        {
            await foreach (var payload in call.ReadResponsesAsync(cancellationToken).ConfigureAwait(false))
                yield return _codec.Decode<TResp>(payload);
        }
        finally
        {
            sendCts.Cancel();
            // Source MoveNext/Dispose may ignore cancellation. The RPC still ends.
            ObserveDetached(sending);
        }
    }

    private static void ObserveDetached(Task task)
    {
        _ = task.ContinueWith(static failed => { _ = failed.Exception; }, CancellationToken.None,
            TaskContinuationOptions.OnlyOnFaulted | TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
    }

    public ValueTask DisposeAsync()
    {
        if (!_ownsConnection) return ValueTask.CompletedTask;
        try { _connection.Close(); }
        catch (IrohException) { /* already closed */ }
        return ValueTask.CompletedTask;
    }

    internal static Exception CallError(int code, string what) => code switch
    {
        (int)Status.NotFound => new RpcError(StatusCode.Unavailable, $"{what}: connection or call is gone"),
        (int)Status.InvalidArgument => new RpcError(StatusCode.InvalidArgument, IrohException.FromStatus(code, what).Message),
        _ => new RpcError(StatusCode.Unavailable, IrohException.FromStatus(code, what).Message),
    };

    /// <summary>The <see cref="RpcError"/> for a failed call's code and AEv1 <c>RpcStatus</c> bytes.</summary>
    private static RpcError StatusError(int code, byte[] status)
    {
        RpcStatus st = status.Length > 0 ? AsterEnvelope.DecodeRpcStatus(status) : new RpcStatus((StatusCode)code);
        if (st.Code == StatusCode.Ok) st = st with { Code = (StatusCode)code };
        return new RpcError(st.Code, st.Message, st.Details.GroupBy(kv => kv.Key).ToDictionary(g => g.Key, g => g.Last().Value));
    }

    /// <summary>One call on its own stream (<c>aster_rpc_open</c>); a cancelled token cancels it natively.</summary>
    private sealed class PayloadCall : IAsyncDisposable
    {
        private const int BatchBytes = 64 * 1024;
        private const int BatchCount = 256;

        private readonly RpcConnection _owner;
        private readonly ulong _call;
        private CancellationTokenRegistration _cancel;
        private int _released;
        private readonly object _operationGate = new();
        private int _activeOperations;
        private ExceptionDispatchInfo? _producerError;

        private PayloadCall(RpcConnection owner, ulong call)
        {
            _owner = owner;
            _call = call;
        }

        public static async Task<PayloadCall> OpenAsync(RpcConnection owner, byte pattern, CallSpec spec, CancellationToken ct)
        {
            ct.ThrowIfCancellationRequested();
            // Server-stream defers the stream open to the first send; client-stream / bidi open
            // the stream and send the header now (a network round trip: off the caller's thread).
            ulong handle = pattern == PatternServerStream
                ? owner.Open(pattern, spec)
                : await Task.Run(() => owner.Open(pattern, spec), CancellationToken.None).ConfigureAwait(false);
            var call = new PayloadCall(owner, handle);
            if (ct.CanBeCanceled)
                call._cancel = ct.UnsafeRegister(static s => ((PayloadCall)s!).CancelNative(), call);
            return call;
        }

        private bool BeginOperation()
        {
            lock (_operationGate)
            {
                if (_released != 0) return false;
                _activeOperations++;
                return true;
            }
        }

        private void EndOperation()
        {
            lock (_operationGate)
            {
                _activeOperations--;
                Monitor.PulseAll(_operationGate);
            }
        }

        private void CancelNative()
        {
            if (!BeginOperation()) return;
            try { Native.aster_call_cancel(_owner.Rt, _call); }
            finally { EndOperation(); }
        }

        /// <summary>Sends one request payload; false when the call is over (the trailer says why).</summary>
        public unsafe bool Send(byte[] payload, bool end)
        {
            if (!BeginOperation()) return false;
            try { return SendCore(payload, end); }
            finally { EndOperation(); }
        }

        private unsafe bool SendCore(byte[] payload, bool end)
        {
            int r;
            fixed (byte* p = payload)
                r = Native.aster_call_send_payload(_owner.Rt, _call, payload.Length == 0 ? null : p, (uint)payload.Length, end ? (byte)1 : (byte)0);
            if (r == (int)Status.AlreadyClosed) return false;
            if (r != 0) throw CallError(r, "aster_call_send_payload");
            return true;
        }

        /// <summary>Several payloads framed into as few writes as possible (client-stream / bidi).</summary>
        private unsafe bool SendMany(List<byte[]> payloads, bool end)
        {
            if (!BeginOperation()) return false;
            try { return SendManyCore(payloads, end); }
            finally { EndOperation(); }
        }

        private unsafe bool SendManyCore(List<byte[]> payloads, bool end)
        {
            if (payloads.Count <= 1)
                return SendCore(payloads.Count == 0 ? Array.Empty<byte>() : payloads[0], end);
            var pins = new GCHandle[payloads.Count];
            var descs = new Bytes[payloads.Count];
            try
            {
                for (int i = 0; i < payloads.Count; i++)
                {
                    pins[i] = GCHandle.Alloc(payloads[i], GCHandleType.Pinned);
                    descs[i] = new Bytes { ptr = pins[i].AddrOfPinnedObject(), len = (UIntPtr)payloads[i].Length };
                }
                int r;
                fixed (Bytes* d = descs)
                    r = Native.aster_call_send_payloads(_owner.Rt, _call, d, (uint)descs.Length, end ? (byte)1 : (byte)0);
                if (r == (int)Status.AlreadyClosed) return false;
                if (r != 0) throw CallError(r, "aster_call_send_payloads");
                return true;
            }
            finally
            {
                foreach (var h in pins)
                    if (h.IsAllocated) h.Free();
            }
        }

        /// <summary>
        /// Sends every request and ends the request stream. Requests that are ready together are
        /// batched into one native call; the batch is flushed before waiting for the next one.
        /// </summary>
        public async Task SendAllAsync<TReq>(IAsyncEnumerable<TReq> requests, ICodec codec, CancellationToken ct)
        {
            var batch = new List<byte[]>();
            int bytes = 0;
            IAsyncEnumerator<TReq>? e = null;
            Task<bool>? pending = null;
            try
            {
                e = requests.GetAsyncEnumerator(ct);
                while (true)
                {
                    ct.ThrowIfCancellationRequested();
                    ValueTask<bool> next = e.MoveNextAsync();
                    pending = next.IsCompleted ? null : next.AsTask();
                    if (pending is not null && batch.Count > 0)
                    {
                        if (!SendMany(batch, end: false)) return;
                        batch.Clear();
                        bytes = 0;
                    }
                    bool more = pending is null ? next.GetAwaiter().GetResult()
                        : await pending.WaitAsync(ct).ConfigureAwait(false);
                    pending = null;
                    ct.ThrowIfCancellationRequested();
                    if (!more) break;
                    byte[] payload = codec.Encode(e.Current);
                    ct.ThrowIfCancellationRequested();
                    batch.Add(payload);
                    bytes += payload.Length;
                    if (bytes >= BatchBytes || batch.Count >= BatchCount)
                    {
                        if (!SendMany(batch, end: false)) return;
                        batch.Clear();
                        bytes = 0;
                    }
                }
                ct.ThrowIfCancellationRequested();
                SendMany(batch, end: true);
            }
            catch (Exception error)
            {
                if (!ct.IsCancellationRequested)
                    Volatile.Write(ref _producerError, ExceptionDispatchInfo.Capture(error));
                CancelNative();
                throw;
            }
            finally
            {
                if (e is not null && pending is { IsCompleted: false })
                {
                    // Enumerators need not support Dispose concurrently with MoveNext.
                    // Await that outstanding user operation only in detached cleanup.
                    async Task Cleanup()
                    {
                        try { await pending.ConfigureAwait(false); } catch { }
                        await e.DisposeAsync().ConfigureAwait(false);
                    }
                    ObserveDetached(Cleanup());
                }
                else if (e is not null)
                {
                    try { await e.DisposeAsync().ConfigureAwait(false); }
                    catch (Exception error)
                    {
                        if (!ct.IsCancellationRequested)
                            Volatile.Write(ref _producerError, ExceptionDispatchInfo.Capture(error));
                        CancelNative();
                        throw;
                    }
                }
            }
        }

        private enum RecvKind { Payload, Trailer, Timeout, Closed }

        private unsafe (RecvKind, byte[]) Recv(uint timeoutMs)
        {
            byte* ptr = null;
            uint len = 0;
            ulong buffer = 0;
            byte trailer = 0;
            int r = Native.aster_call_recv_payload(_owner.Rt, _call, timeoutMs, &ptr, &len, &buffer, &trailer);
            if (r == (int)Status.Timeout) return (RecvKind.Timeout, Array.Empty<byte>());
            if (r == (int)Status.AlreadyClosed) return (RecvKind.Closed, Array.Empty<byte>());
            if (r != 0) throw CallError(r, "aster_call_recv_payload");
            try
            {
                byte[] bytes = len == 0 ? Array.Empty<byte>() : new ReadOnlySpan<byte>(ptr, (int)len).ToArray();
                return (trailer != 0 ? RecvKind.Trailer : RecvKind.Payload, bytes);
            }
            finally
            {
                if (buffer != 0) Native.aster_call_buffer_release(_owner.Rt, buffer);
            }
        }

        private (RecvKind, byte[]) WaitRecv()
        {
            while (true)
            {
                var r = Recv(RecvWaitMs);
                if (r.Item1 != RecvKind.Timeout) return r;
            }
        }

        /// <summary>Response payloads until the trailer; a failed call throws <see cref="RpcError"/>.</summary>
        public async IAsyncEnumerable<byte[]> ReadResponsesAsync([EnumeratorCancellation] CancellationToken ct)
        {
            while (true)
            {
                // Buffered payloads are taken without a thread hop; an empty queue waits on the
                // thread pool (a cancelled token cancels the call, which ends the wait).
                var (kind, bytes) = Recv(0);
                if (kind == RecvKind.Timeout)
                    (kind, bytes) = await Task.Run(WaitRecv, CancellationToken.None).ConfigureAwait(false);
                Volatile.Read(ref _producerError)?.Throw();
                switch (kind)
                {
                    case RecvKind.Payload:
                        yield return bytes;
                        continue;
                    case RecvKind.Trailer:
                        ct.ThrowIfCancellationRequested();
                        var status = bytes.Length == 0 ? RpcStatus.Ok : AsterEnvelope.DecodeRpcStatus(bytes);
                        if (status.Code != StatusCode.Ok) throw StatusError((int)status.Code, bytes);
                        yield break;
                    default:
                        ct.ThrowIfCancellationRequested();
                        throw new RpcError(StatusCode.Unavailable, "call already closed");
                }
            }
        }

        /// <summary>Reads the single response of a unary / client-stream call.</summary>
        public async Task<byte[]> ReadSingleAsync(CancellationToken ct)
        {
            byte[]? response = null;
            int count = 0;
            await foreach (var payload in ReadResponsesAsync(ct).ConfigureAwait(false))
            {
                response = payload;
                count++;
            }
            if (count > 1)
                throw new RpcError(StatusCode.Internal, $"single-response call received {count} response frames");
            return response ?? Array.Empty<byte>();
        }

        public ValueTask DisposeAsync()
        {
            // The owning response reader is drained before disposal. Stop new send/cancel
            // operations, wake active sends, and drain their leases before native release.
            _cancel.Dispose();
            lock (_operationGate)
            {
                if (_released != 0) return ValueTask.CompletedTask;
                _released = 1;
            }
            // Cancel outside the gate: a flow-controlled send needs cancellation to drain.
            Native.aster_call_cancel(_owner.Rt, _call);
            lock (_operationGate)
            {
                while (_activeOperations != 0) Monitor.Wait(_operationGate);
                Native.aster_call_release(_owner.Rt, _call);
            }
            return ValueTask.CompletedTask;
        }
    }

    private unsafe ulong Open(byte pattern, CallSpec spec)
    {
        int r;
        ulong handle;
        fixed (byte* sp = spec.Service, mp = spec.Method, kp = spec.Metadata)
        {
            CallOptionsNative o = spec.Native(sp, mp, kp);
            r = Native.aster_rpc_open(Rt, Conn, pattern, &o, out handle);
        }
        if (r != 0) throw CallError(r, "aster_rpc_open");
        return handle;
    }
}

internal static class AsyncEnumerableExtensions
{
    public static async IAsyncEnumerable<T> ToAsyncEnumerable<T>(this IEnumerable<T> source)
    {
        foreach (var item in source)
        {
            yield return item;
            await Task.CompletedTask.ConfigureAwait(false);
        }
    }
}
