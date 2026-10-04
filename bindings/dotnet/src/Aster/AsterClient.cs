using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Codec;
using Aster.Interceptors;
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

    /// <summary>Request metadata (stream header).</summary>
    public IReadOnlyList<KeyValuePair<string, string>>? Metadata { get; init; }

    /// <summary>
    /// 0 = shared service; non-zero = a session of a session-scoped service (take ids from
    /// <see cref="RpcConnection.NextSessionId"/>). Unary calls of one session share one pooled stream.
    /// </summary>
    public int SessionId { get; init; }

    /// <summary>Service version (<c>StreamHeader.version</c>).</summary>
    public int Version { get; init; } = 1;

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
/// Calls go straight to <c>aster/1</c> (open gate); consumer admission is not performed.
/// </summary>
public sealed class AsterClient : IAsyncDisposable
{
    private readonly Node _node;
    private readonly bool _ownsNode;
    private readonly ConcurrentDictionary<string, Task<RpcConnection>> _connections = new();

    private AsterClient(Node node, ICodec codec, bool ownsNode)
    {
        _node = node;
        Codec = codec;
        _ownsNode = ownsNode;
    }

    /// <summary>Starts a client node (adds the <c>aster/1</c> ALPN to <paramref name="config"/>).</summary>
    public static async Task<AsterClient> StartAsync(AsterConfig? config = null, ICodec? codec = null, CancellationToken cancellationToken = default)
    {
        var node = await Node.StartAsync(config, null, [AsterServer.AsterAlpn], cancellationToken).ConfigureAwait(false);
        return new AsterClient(node, codec ?? new ForyCodec(), ownsNode: true);
    }

    /// <summary>A client on an existing node (which the caller keeps owning).</summary>
    public static AsterClient Create(Node node, ICodec? codec = null) => new(node, codec ?? new ForyCodec(), ownsNode: false);

    public Node Node => _node;

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

    private async Task<RpcConnection> ConnectCore(NodeAddrInfo addr, CancellationToken ct)
    {
        var conn = await _node.ConnectAsync(addr, AsterServer.AsterAlpn, ct).ConfigureAwait(false);
        return new RpcConnection(conn, Codec);
    }

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

/// <summary>
/// RPC calls over one QUIC connection through the payload-level C ABI client (3.5,
/// <c>aster_rpc_*</c> / <c>aster_call_*_payload</c>) on Rust's pooled <c>RpcConnection</c>: Rust
/// builds the stream header, frames, compresses, enforces the deadline and decodes trailers;
/// .NET passes options and payloads. Unary calls use the pooled streams; a unary call with a
/// cancellable token, and every streaming call, get their own stream so that cancelling the token
/// cancels the native call (<c>aster_call_cancel</c>: the server sees the cancel). Blocking
/// native calls run on the thread pool.
/// </summary>
public sealed class RpcConnection : IAsyncDisposable
{
    private const uint RecvWaitMs = 250;
    private const byte PatternServerStream = 1;
    private const byte PatternClientStream = 2;
    private const byte PatternBidiStream = 3;

    private readonly Connection _connection;
    private readonly ICodec _codec;

    internal RpcConnection(Connection connection, ICodec codec)
    {
        _connection = connection;
        _codec = codec;
    }

    public Connection Connection => _connection;

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
        public readonly byte[] Metadata = options?.Metadata is { Count: > 0 } md ? KvList.Encode(md) : Array.Empty<byte>();
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
    public async Task<TResp> UnaryAsync<TReq, TResp>(string service, string method, TReq request,
        CallOptions? options = null, CancellationToken cancellationToken = default)
    {
        byte[] req = _codec.Encode(request);
        var spec = new CallSpec(service, method, options);
        byte[] response;
        if (!cancellationToken.CanBeCanceled)
        {
            // Pooled fast path: one blocking native call (header + request in one write).
            var (code, payload, status) = await Task.Run(() => UnaryNative(spec, req), CancellationToken.None).ConfigureAwait(false);
            if (code != 0) throw StatusError(code, status);
            response = payload;
        }
        else
        {
            cancellationToken.ThrowIfCancellationRequested();
            // Same wire shape on a dedicated stream, so the token can cancel the native call.
            await using var call = await PayloadCall.OpenAsync(this, PatternServerStream, spec, cancellationToken).ConfigureAwait(false);
            call.Send(req, end: true);
            response = await call.ReadSingleAsync(cancellationToken).ConfigureAwait(false);
        }
        if (response.Length == 0)
            throw new RpcError(StatusCode.Internal, "OK trailer with no response frame");
        return _codec.Decode<TResp>(response);
    }

    private unsafe (int Code, byte[] Payload, byte[] Status) UnaryNative(CallSpec spec, byte[] request)
    {
        byte* respPtr = null, statusPtr = null;
        uint respLen = 0, statusLen = 0;
        ulong respBuf = 0, statusBuf = 0;
        int code = 0;
        int r;
        fixed (byte* sp = spec.Service, mp = spec.Method, kp = spec.Metadata, rp = request)
        {
            CallOptionsNative o = spec.Native(sp, mp, kp);
            r = Native.aster_rpc_unary(Rt, Conn, &o, request.Length == 0 ? null : rp, (uint)request.Length,
                &respPtr, &respLen, &respBuf, &code, &statusPtr, &statusLen, &statusBuf);
        }
        if (r != 0) throw CallError(r, "aster_rpc_unary");
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
    public async IAsyncEnumerable<TResp> ServerStreamAsync<TReq, TResp>(string service, string method, TReq request,
        CallOptions? options = null, [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        await using var call = await PayloadCall.OpenAsync(this, PatternServerStream, new CallSpec(service, method, options), cancellationToken)
            .ConfigureAwait(false);
        call.Send(_codec.Encode(request), end: true);
        await foreach (var payload in call.ReadResponsesAsync(cancellationToken).ConfigureAwait(false))
            yield return _codec.Decode<TResp>(payload);
    }

    /// <summary>A client-streaming call: a stream of requests, one response.</summary>
    public async Task<TResp> ClientStreamAsync<TReq, TResp>(string service, string method, IAsyncEnumerable<TReq> requests,
        CallOptions? options = null, CancellationToken cancellationToken = default)
    {
        await using var call = await PayloadCall.OpenAsync(this, PatternClientStream, new CallSpec(service, method, options), cancellationToken)
            .ConfigureAwait(false);
        await call.SendAllAsync(requests, _codec, cancellationToken).ConfigureAwait(false);
        byte[] response = await call.ReadSingleAsync(cancellationToken).ConfigureAwait(false);
        if (response.Length == 0)
            throw new RpcError(StatusCode.Internal, "OK trailer with no response frame");
        return _codec.Decode<TResp>(response);
    }

    /// <inheritdoc cref="ClientStreamAsync{TReq,TResp}(string,string,IAsyncEnumerable{TReq},CallOptions?,CancellationToken)"/>
    public Task<TResp> ClientStreamAsync<TReq, TResp>(string service, string method, IEnumerable<TReq> requests,
        CallOptions? options = null, CancellationToken cancellationToken = default)
        => ClientStreamAsync<TReq, TResp>(service, method, requests.ToAsyncEnumerable(), options, cancellationToken);

    /// <summary>A bidi-streaming call: requests are sent concurrently with reading responses.</summary>
    public async IAsyncEnumerable<TResp> BidiStreamAsync<TReq, TResp>(string service, string method, IAsyncEnumerable<TReq> requests,
        CallOptions? options = null, [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        await using var call = await PayloadCall.OpenAsync(this, PatternBidiStream, new CallSpec(service, method, options), cancellationToken)
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
            try { await sending.ConfigureAwait(false); }
            catch (OperationCanceledException) { }
        }
    }

    public ValueTask DisposeAsync()
    {
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

        private void CancelNative()
        {
            if (Volatile.Read(ref _released) == 0)
                Native.aster_call_cancel(_owner.Rt, _call);
        }

        /// <summary>Sends one request payload; false when the call is over (the trailer says why).</summary>
        public unsafe bool Send(byte[] payload, bool end)
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
            if (payloads.Count <= 1)
                return Send(payloads.Count == 0 ? Array.Empty<byte>() : payloads[0], end);
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
            await using var e = requests.GetAsyncEnumerator(ct);
            while (true)
            {
                ValueTask<bool> next = e.MoveNextAsync();
                if (!next.IsCompleted && batch.Count > 0)
                {
                    if (!SendMany(batch, end: false)) return;
                    batch.Clear();
                    bytes = 0;
                }
                if (!await next.ConfigureAwait(false)) break;
                byte[] payload = codec.Encode(e.Current);
                batch.Add(payload);
                bytes += payload.Length;
                if (bytes >= BatchBytes || batch.Count >= BatchCount)
                {
                    if (!SendMany(batch, end: false)) return;
                    batch.Clear();
                    bytes = 0;
                }
            }
            SendMany(batch, end: true);
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
            // Waits for a running cancel callback, then frees the call (cancelling it if unfinished).
            _cancel.Dispose();
            if (Interlocked.Exchange(ref _released, 1) == 0)
                Native.aster_call_release(_owner.Rt, _call);
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
