using System;
using System.Collections.Generic;
using System.Reflection;
using System.Runtime.CompilerServices;
using System.Threading;
using System.Threading.Tasks;
using Aster.Codec;

namespace Aster.Rpc;

/// <summary>RPC method shape (contract <c>MethodPattern</c>).</summary>
public enum MethodPattern
{
    Unary,
    ServerStream,
    ClientStream,
    BidiStream,
}

/// <summary>Service instance scope (contract <c>ScopeKind</c>).</summary>
public enum ServiceScope
{
    /// <summary>One implementation serves every caller.</summary>
    Shared,
    /// <summary>
    /// One session per (connection, session id): Rust routes the calls, enforces the
    /// per-connection session cap and reaps sessions; handlers reach the per-session state via
    /// <see cref="ServerCallContext.Session"/> (created by
    /// <see cref="ServiceDefinition.Builder.SessionFactory"/>).
    /// </summary>
    Session,
}

/// <summary>Kind of a <see cref="CapabilityRequirement"/> (contract <c>CapabilityKind</c>).</summary>
public enum CapabilityKind
{
    /// <summary>The caller holds the single listed role.</summary>
    Role,
    /// <summary>The caller holds at least one of the roles.</summary>
    AnyOf,
    /// <summary>The caller holds every role.</summary>
    AllOf,
}

/// <summary>
/// A Gate-3 role requirement on a service or method (contract <c>requires</c>). Rust checks it
/// against the caller's attributes (<c>aster.role</c>; see <see cref="AsterServer.SetPeerAttributes"/>)
/// before the call reaches .NET, and it is part of the contract id.
/// </summary>
public sealed record CapabilityRequirement(CapabilityKind Kind, IReadOnlyList<string> Roles)
{
    public static CapabilityRequirement Role(string role) => new(CapabilityKind.Role, [role]);
    public static CapabilityRequirement AnyOf(params string[] roles) => new(CapabilityKind.AnyOf, roles);
    public static CapabilityRequirement AllOf(params string[] roles) => new(CapabilityKind.AllOf, roles);

    internal string WireKind => Kind switch
    {
        CapabilityKind.Role => "role",
        CapabilityKind.AnyOf => "any_of",
        _ => "all_of",
    };
}

/// <summary>Per-call information handed to a server handler.</summary>
public sealed class ServerCallContext
{
    internal ServerCallContext(string service, string method, MethodPattern pattern, string peerId, ulong connectionId,
        int sessionId, ulong sessionKey, object? session,
        IReadOnlyList<KeyValuePair<string, string>> metadata, IReadOnlyDictionary<string, string> attributes,
        DateTimeOffset? deadline, CancellationToken cancellationToken)
    {
        Service = service;
        Method = method;
        Pattern = pattern;
        PeerId = peerId;
        ConnectionId = connectionId;
        SessionId = sessionId;
        SessionKey = sessionKey;
        Session = session;
        Metadata = metadata;
        Attributes = attributes;
        Deadline = deadline;
        CancellationToken = cancellationToken;
    }

    public string Service { get; }
    public string Method { get; }
    public MethodPattern Pattern { get; }

    /// <summary>The caller's node id (64 hex).</summary>
    public string PeerId { get; }

    /// <summary>Server connection id: key per-connection state on it (see <see cref="AsterServer.ConnectionClosed"/>).</summary>
    public ulong ConnectionId { get; }

    /// <summary>The wire session id (0 = shared service); for logs. Rust keys the session by <see cref="SessionKey"/>.</summary>
    public int SessionId { get; }

    /// <summary>Rust's key of this call's session (0 for a shared service); unique per server.</summary>
    public ulong SessionKey { get; }

    /// <summary>
    /// The per-session state of a session-scoped service (from its
    /// <see cref="ServiceDefinition.Builder.SessionFactory"/>); null for a shared service or when
    /// no factory was given.
    /// </summary>
    public object? Session { get; }

    /// <summary>Request metadata from the stream header.</summary>
    public IReadOnlyList<KeyValuePair<string, string>> Metadata { get; }

    /// <summary>The caller's attributes (what Gate 3 saw; e.g. <c>aster.role</c>).</summary>
    public IReadOnlyDictionary<string, string> Attributes { get; }

    /// <summary>When Rust cancels the call with <c>DEADLINE_EXCEEDED</c> (caller deadline, method default or handler cap).</summary>
    public DateTimeOffset? Deadline { get; }

    /// <summary>Cancelled when the deadline passes, the peer cancels, or the server stops.</summary>
    public CancellationToken CancellationToken { get; }
}

/// <summary>One method of a <see cref="ServiceDefinition"/>.</summary>
public abstract class MethodDefinition
{
    private protected MethodDefinition(string name, MethodPattern pattern, Type requestType, Type responseType, bool idempotent,
        CapabilityRequirement? requires, TimeSpan? defaultTimeout)
    {
        if (defaultTimeout is { } t && t < TimeSpan.Zero)
            throw new ArgumentOutOfRangeException(nameof(defaultTimeout), "the default timeout must not be negative");
        Name = name;
        Pattern = pattern;
        RequestType = requestType;
        ResponseType = responseType;
        Idempotent = idempotent;
        Requires = requires;
        DefaultTimeout = defaultTimeout;
    }

    public string Name { get; }
    public MethodPattern Pattern { get; }
    public Type RequestType { get; }
    public Type ResponseType { get; }
    public bool Idempotent { get; }

    /// <summary>Gate-3 requirement on this method (in the contract; enforced by Rust).</summary>
    public CapabilityRequirement? Requires { get; }

    /// <summary>Default deadline when the caller sets none (in the contract; enforced by Rust).</summary>
    public TimeSpan? DefaultTimeout { get; }

    internal abstract Task InvokeAsync(ServerCall call);

    internal abstract void RegisterTypes(ForyCodec codec);
}

/// <summary>
/// A service served by <see cref="AsterServer"/>: name, version and typed handlers. Build one
/// with <see cref="Create"/>:
/// <code>
/// var service = ServiceDefinition.Create("InteropEcho", 1)
///     .Unary&lt;EchoRequest, EchoReply&gt;("echo", (req, ctx) => ValueTask.FromResult(...))
///     .ServerStream&lt;EchoRequest, EchoReply&gt;("generate", Generate)
///     .Build();
/// </code>
/// Request and response types are Fory structs carrying <see cref="WireTypeAttribute"/>; the
/// server registers them with its <see cref="ForyCodec"/> and derives the contract id from them.
/// </summary>
public sealed class ServiceDefinition
{
    private string? _contractId;
    private string? _registrationJson;

    internal ServiceDefinition(string name, int version, ServiceScope scope, IReadOnlyDictionary<string, MethodDefinition> methods,
        CapabilityRequirement? requires, Func<string, object>? sessionFactory)
    {
        Name = name;
        Version = version;
        Scope = scope;
        Methods = methods;
        Requires = requires;
        SessionFactory = sessionFactory;
        OrderedMethods = ContractBuilder.Ordered(methods.Values);
    }

    public string Name { get; }
    public int Version { get; }
    public ServiceScope Scope { get; }
    public IReadOnlyDictionary<string, MethodDefinition> Methods { get; }

    /// <summary>Service-wide Gate-3 requirement (in the contract; enforced by Rust).</summary>
    public CapabilityRequirement? Requires { get; }

    /// <summary>Per-session state factory (session scope), given the caller's node id.</summary>
    public Func<string, object>? SessionFactory { get; }

    /// <summary>Methods in contract order: the index is the dispatch <c>method_idx</c>.</summary>
    internal MethodDefinition[] OrderedMethods { get; }

    /// <summary>The contract id (BLAKE3 of the canonical <c>ServiceContract</c>, computed in Rust).</summary>
    public string ContractId => _contractId ??= ContractBuilder.ComputeContractId(this);

    /// <summary>
    /// The JSON registration form (<c>DynamicService</c>, dynamic-dispatch design §1.5) the server
    /// registers with <c>aster_server_add_service_json</c>; its methods are in
    /// <see cref="OrderedMethods"/> order.
    /// </summary>
    public string RegistrationJson => _registrationJson ??= ContractBuilder.RegistrationJson(this);

    public static Builder Create(string name, int version = 1, ServiceScope scope = ServiceScope.Shared) => new(name, version, scope);

    /// <summary>Fluent builder for a <see cref="ServiceDefinition"/>.</summary>
    public sealed class Builder
    {
        private readonly string _name;
        private readonly int _version;
        private readonly ServiceScope _scope;
        private readonly Dictionary<string, MethodDefinition> _methods = new(StringComparer.Ordinal);
        private CapabilityRequirement? _requires;
        private Func<string, object>? _sessionFactory;

        internal Builder(string name, int version, ServiceScope scope)
        {
            ArgumentException.ThrowIfNullOrEmpty(name);
            _name = name;
            _version = version;
            _scope = scope;
        }

        public Builder Unary<TReq, TResp>(string method, Func<TReq, ServerCallContext, ValueTask<TResp>> handler, bool idempotent = false,
            CapabilityRequirement? requires = null, TimeSpan? defaultTimeout = null)
            => Add(new UnaryMethod<TReq, TResp>(method, handler, idempotent, requires, defaultTimeout));

        public Builder ServerStream<TReq, TResp>(string method, Func<TReq, ServerCallContext, IAsyncEnumerable<TResp>> handler,
            CapabilityRequirement? requires = null, TimeSpan? defaultTimeout = null)
            => Add(new ServerStreamMethod<TReq, TResp>(method, handler, requires, defaultTimeout));

        public Builder ClientStream<TReq, TResp>(string method, Func<IAsyncEnumerable<TReq>, ServerCallContext, ValueTask<TResp>> handler,
            CapabilityRequirement? requires = null, TimeSpan? defaultTimeout = null)
            => Add(new ClientStreamMethod<TReq, TResp>(method, handler, requires, defaultTimeout));

        public Builder BidiStream<TReq, TResp>(string method, Func<IAsyncEnumerable<TReq>, ServerCallContext, IAsyncEnumerable<TResp>> handler,
            CapabilityRequirement? requires = null, TimeSpan? defaultTimeout = null)
            => Add(new BidiStreamMethod<TReq, TResp>(method, handler, requires, defaultTimeout));

        /// <summary>A Gate-3 requirement on every method of the service.</summary>
        public Builder Requires(CapabilityRequirement requirement)
        {
            _requires = requirement;
            return this;
        }

        /// <summary>
        /// Session scope: creates the per-session state on a session's first call (given the
        /// caller's node id), exposed as <see cref="ServerCallContext.Session"/>. When Rust reaps
        /// the session (client close, connection close, cap) the state is disposed if it is
        /// <see cref="IAsyncDisposable"/> / <see cref="IDisposable"/>.
        /// </summary>
        public Builder SessionFactory(Func<string, object> factory)
        {
            if (_scope != ServiceScope.Session)
                throw new InvalidOperationException("a session factory needs ServiceScope.Session");
            _sessionFactory = factory ?? throw new ArgumentNullException(nameof(factory));
            return this;
        }

        private Builder Add(MethodDefinition m)
        {
            if (!_methods.TryAdd(m.Name, m))
                throw new ArgumentException($"duplicate method {_name}.{m.Name}");
            return this;
        }

        public ServiceDefinition Build()
            => new(_name, _version, _scope, new Dictionary<string, MethodDefinition>(_methods), _requires, _sessionFactory);
    }
}

internal abstract class TypedMethod<TReq, TResp> : MethodDefinition
{
    protected TypedMethod(string name, MethodPattern pattern, bool idempotent, CapabilityRequirement? requires, TimeSpan? defaultTimeout)
        : base(name, pattern, typeof(TReq), typeof(TResp), idempotent, requires, defaultTimeout) { }

    internal override void RegisterTypes(ForyCodec codec)
    {
        Register<TReq>(codec);
        Register<TResp>(codec);
    }

    internal static void Register<T>(ForyCodec codec)
    {
        if (typeof(T).GetCustomAttribute<WireTypeAttribute>() is not { } wire) return;
        try { codec.Register<T>(wire.Tag); }
        catch (Exception) { /* already registered */ }
    }

    protected static async IAsyncEnumerable<TReq> Decode(ServerCall call, [EnumeratorCancellation] CancellationToken ct = default)
    {
        await foreach (var payload in call.ReadRequestsAsync(ct).ConfigureAwait(false))
            yield return call.Codec.Decode<TReq>(payload);
    }
}

internal sealed class UnaryMethod<TReq, TResp>(string name, Func<TReq, ServerCallContext, ValueTask<TResp>> handler, bool idempotent,
    CapabilityRequirement? requires, TimeSpan? defaultTimeout)
    : TypedMethod<TReq, TResp>(name, MethodPattern.Unary, idempotent, requires, defaultTimeout)
{
    internal override async Task InvokeAsync(ServerCall call)
    {
        var request = call.Codec.Decode<TReq>(call.FirstRequest);
        TResp response = await handler(request, call.Context).ConfigureAwait(false);
        await call.CompleteUnaryAsync(call.Codec.Encode(response)).ConfigureAwait(false);
    }
}

internal sealed class ServerStreamMethod<TReq, TResp>(string name, Func<TReq, ServerCallContext, IAsyncEnumerable<TResp>> handler,
    CapabilityRequirement? requires, TimeSpan? defaultTimeout)
    : TypedMethod<TReq, TResp>(name, MethodPattern.ServerStream, false, requires, defaultTimeout)
{
    internal override async Task InvokeAsync(ServerCall call)
    {
        var request = call.Codec.Decode<TReq>(call.FirstRequest);
        await foreach (var item in handler(request, call.Context).WithCancellation(call.Context.CancellationToken).ConfigureAwait(false))
            await call.SendAsync(call.Codec.Encode(item)).ConfigureAwait(false);
        await call.CompleteAsync(RpcStatus.Ok).ConfigureAwait(false);
    }
}

internal sealed class ClientStreamMethod<TReq, TResp>(string name, Func<IAsyncEnumerable<TReq>, ServerCallContext, ValueTask<TResp>> handler,
    CapabilityRequirement? requires, TimeSpan? defaultTimeout)
    : TypedMethod<TReq, TResp>(name, MethodPattern.ClientStream, false, requires, defaultTimeout)
{
    internal override async Task InvokeAsync(ServerCall call)
    {
        TResp response = await handler(Decode(call), call.Context).ConfigureAwait(false);
        await call.CompleteUnaryAsync(call.Codec.Encode(response)).ConfigureAwait(false);
    }
}

internal sealed class BidiStreamMethod<TReq, TResp>(string name, Func<IAsyncEnumerable<TReq>, ServerCallContext, IAsyncEnumerable<TResp>> handler,
    CapabilityRequirement? requires, TimeSpan? defaultTimeout)
    : TypedMethod<TReq, TResp>(name, MethodPattern.BidiStream, false, requires, defaultTimeout)
{
    internal override async Task InvokeAsync(ServerCall call)
    {
        await foreach (var item in handler(Decode(call), call.Context).WithCancellation(call.Context.CancellationToken).ConfigureAwait(false))
            await call.SendAsync(call.Codec.Encode(item)).ConfigureAwait(false);
        await call.CompleteAsync(RpcStatus.Ok).ConfigureAwait(false);
    }
}
