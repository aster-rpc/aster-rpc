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
    /// <summary>One instance per (connection, session id); not served by the .NET server yet.</summary>
    Session,
}

/// <summary>Per-call information handed to a server handler.</summary>
public sealed class ServerCallContext
{
    internal ServerCallContext(StreamHeader header, MethodPattern pattern, string peerId, ulong connectionId,
        DateTimeOffset? deadline, CancellationToken cancellationToken)
    {
        Service = header.Service;
        Method = header.Method;
        Metadata = header.Metadata;
        SessionId = header.SessionId;
        Pattern = pattern;
        PeerId = peerId;
        ConnectionId = connectionId;
        Deadline = deadline;
        CancellationToken = cancellationToken;
    }

    public string Service { get; }
    public string Method { get; }
    public MethodPattern Pattern { get; }

    /// <summary>The caller's node id (64 hex).</summary>
    public string PeerId { get; }

    /// <summary>Reactor connection id: key per-connection state on it.</summary>
    public ulong ConnectionId { get; }

    public int SessionId { get; }

    /// <summary>Request metadata from the stream header.</summary>
    public IReadOnlyList<KeyValuePair<string, string>> Metadata { get; }

    /// <summary>When the caller's deadline expires (whole seconds on the wire), if it set one.</summary>
    public DateTimeOffset? Deadline { get; }

    /// <summary>Cancelled when the deadline passes, the peer cancels, or the server stops.</summary>
    public CancellationToken CancellationToken { get; }
}

/// <summary>One method of a <see cref="ServiceDefinition"/>.</summary>
public abstract class MethodDefinition
{
    private protected MethodDefinition(string name, MethodPattern pattern, Type requestType, Type responseType, bool idempotent)
    {
        Name = name;
        Pattern = pattern;
        RequestType = requestType;
        ResponseType = responseType;
        Idempotent = idempotent;
    }

    public string Name { get; }
    public MethodPattern Pattern { get; }
    public Type RequestType { get; }
    public Type ResponseType { get; }
    public bool Idempotent { get; }

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

    internal ServiceDefinition(string name, int version, ServiceScope scope, IReadOnlyDictionary<string, MethodDefinition> methods)
    {
        Name = name;
        Version = version;
        Scope = scope;
        Methods = methods;
    }

    public string Name { get; }
    public int Version { get; }
    public ServiceScope Scope { get; }
    public IReadOnlyDictionary<string, MethodDefinition> Methods { get; }

    /// <summary>The contract id (BLAKE3 of the canonical <c>ServiceContract</c>, computed in Rust).</summary>
    public string ContractId => _contractId ??= ContractBuilder.ComputeContractId(this);

    public static Builder Create(string name, int version = 1, ServiceScope scope = ServiceScope.Shared) => new(name, version, scope);

    /// <summary>Fluent builder for a <see cref="ServiceDefinition"/>.</summary>
    public sealed class Builder
    {
        private readonly string _name;
        private readonly int _version;
        private readonly ServiceScope _scope;
        private readonly Dictionary<string, MethodDefinition> _methods = new(StringComparer.Ordinal);

        internal Builder(string name, int version, ServiceScope scope)
        {
            ArgumentException.ThrowIfNullOrEmpty(name);
            _name = name;
            _version = version;
            _scope = scope;
        }

        public Builder Unary<TReq, TResp>(string method, Func<TReq, ServerCallContext, ValueTask<TResp>> handler, bool idempotent = false)
            => Add(new UnaryMethod<TReq, TResp>(method, handler, idempotent));

        public Builder ServerStream<TReq, TResp>(string method, Func<TReq, ServerCallContext, IAsyncEnumerable<TResp>> handler)
            => Add(new ServerStreamMethod<TReq, TResp>(method, handler));

        public Builder ClientStream<TReq, TResp>(string method, Func<IAsyncEnumerable<TReq>, ServerCallContext, ValueTask<TResp>> handler)
            => Add(new ClientStreamMethod<TReq, TResp>(method, handler));

        public Builder BidiStream<TReq, TResp>(string method, Func<IAsyncEnumerable<TReq>, ServerCallContext, IAsyncEnumerable<TResp>> handler)
            => Add(new BidiStreamMethod<TReq, TResp>(method, handler));

        private Builder Add(MethodDefinition m)
        {
            if (!_methods.TryAdd(m.Name, m))
                throw new ArgumentException($"duplicate method {_name}.{m.Name}");
            return this;
        }

        public ServiceDefinition Build()
        {
            if (_scope == ServiceScope.Session)
                throw new NotSupportedException("session-scoped services are not served by the .NET binding yet");
            return new ServiceDefinition(_name, _version, _scope, new Dictionary<string, MethodDefinition>(_methods));
        }
    }
}

internal abstract class TypedMethod<TReq, TResp> : MethodDefinition
{
    protected TypedMethod(string name, MethodPattern pattern, bool idempotent)
        : base(name, pattern, typeof(TReq), typeof(TResp), idempotent) { }

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

internal sealed class UnaryMethod<TReq, TResp>(string name, Func<TReq, ServerCallContext, ValueTask<TResp>> handler, bool idempotent)
    : TypedMethod<TReq, TResp>(name, MethodPattern.Unary, idempotent)
{
    internal override async Task InvokeAsync(ServerCall call)
    {
        var request = call.Codec.Decode<TReq>(call.FirstRequest);
        TResp response = await handler(request, call.Context).ConfigureAwait(false);
        await call.CompleteUnaryAsync(call.Codec.Encode(response)).ConfigureAwait(false);
    }
}

internal sealed class ServerStreamMethod<TReq, TResp>(string name, Func<TReq, ServerCallContext, IAsyncEnumerable<TResp>> handler)
    : TypedMethod<TReq, TResp>(name, MethodPattern.ServerStream, false)
{
    internal override async Task InvokeAsync(ServerCall call)
    {
        var request = call.Codec.Decode<TReq>(call.FirstRequest);
        await foreach (var item in handler(request, call.Context).WithCancellation(call.Context.CancellationToken).ConfigureAwait(false))
            await call.SendAsync(call.Codec.Encode(item)).ConfigureAwait(false);
        await call.CompleteAsync(RpcStatus.Ok).ConfigureAwait(false);
    }
}

internal sealed class ClientStreamMethod<TReq, TResp>(string name, Func<IAsyncEnumerable<TReq>, ServerCallContext, ValueTask<TResp>> handler)
    : TypedMethod<TReq, TResp>(name, MethodPattern.ClientStream, false)
{
    internal override async Task InvokeAsync(ServerCall call)
    {
        TResp response = await handler(Decode(call), call.Context).ConfigureAwait(false);
        await call.CompleteUnaryAsync(call.Codec.Encode(response)).ConfigureAwait(false);
    }
}

internal sealed class BidiStreamMethod<TReq, TResp>(string name, Func<IAsyncEnumerable<TReq>, ServerCallContext, IAsyncEnumerable<TResp>> handler)
    : TypedMethod<TReq, TResp>(name, MethodPattern.BidiStream, false)
{
    internal override async Task InvokeAsync(ServerCall call)
    {
        await foreach (var item in handler(Decode(call), call.Context).WithCancellation(call.Context.CancellationToken).ConfigureAwait(false))
            await call.SendAsync(call.Codec.Encode(item)).ConfigureAwait(false);
        await call.CompleteAsync(RpcStatus.Ok).ConfigureAwait(false);
    }
}
