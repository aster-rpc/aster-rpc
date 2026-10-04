using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Threading;
using System.Threading.Tasks;
using Aster.Codec;
using Aster.Interceptors;
using Aster.Rpc;

namespace Aster;

/// <summary>Per-call options.</summary>
public sealed record CallOptions
{
    /// <summary>Relative deadline; sent rounded <b>up</b> to whole seconds (max 32767 s).</summary>
    public TimeSpan? Deadline { get; init; }

    /// <summary>Request metadata (stream header).</summary>
    public IReadOnlyList<KeyValuePair<string, string>>? Metadata { get; init; }

    /// <summary>0 = shared service; non-zero = a session of a session-scoped service.</summary>
    public int SessionId { get; init; }

    internal short DeadlineSeconds => Deadline is { } d && d > TimeSpan.Zero
        ? (short)Math.Min(short.MaxValue, Math.Ceiling(d.TotalSeconds))
        : (short)0;
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
/// RPC calls over one QUIC connection, through the C ABI multiplexed-stream pool
/// (<c>aster_call_*</c>): unary calls use the pooled fast path, streaming calls a dedicated
/// substream. Blocking native calls run on the thread pool.
/// </summary>
public sealed class RpcConnection : IAsyncDisposable
{
    private const uint RecvPollMs = 100;

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

    private static byte[] HeaderFrame(string service, string method, CallOptions? options)
        => AsterFraming.EncodeFrame(AsterEnvelope.Encode(new StreamHeader(
            service, method,
            Deadline: options?.DeadlineSeconds ?? 0,
            Metadata: options?.Metadata,
            SessionId: options?.SessionId ?? 0)), AsterFraming.FlagHeader);

    /// <summary>A unary call.</summary>
    public async Task<TResp> UnaryAsync<TReq, TResp>(string service, string method, TReq request,
        CallOptions? options = null, CancellationToken cancellationToken = default)
    {
        byte[] header = HeaderFrame(service, method, options);
        byte[] body = AsterFraming.EncodePayloadFrame(_codec.Encode(request), AsterFraming.FlagEndStream);
        byte[] pair = new byte[header.Length + body.Length];
        header.CopyTo(pair, 0);
        body.CopyTo(pair, header.Length);
        uint sessionId = (uint)(options?.SessionId ?? 0);

        // aster_call_unary blocks its thread for the whole call; a cancelled caller stops
        // waiting but the native call runs to completion (or the transport deadline).
        var native = Task.Run(() => UnaryNative(pair, sessionId), CancellationToken.None);
        var (responsePayload, responseFlags, trailer) = await native.WaitAsync(cancellationToken).ConfigureAwait(false);

        var status = trailer.Length == 0 ? RpcStatus.Ok : AsterEnvelope.DecodeRpcStatus(trailer);
        status.ThrowIfError();
        if (responsePayload is null)
            throw new RpcError(StatusCode.Internal, "OK trailer with no response frame");
        return _codec.Decode<TResp>(AsterFraming.PayloadOf(responsePayload, responseFlags));
    }

    private unsafe (byte[]? Payload, byte Flags, byte[] Trailer) UnaryNative(byte[] pair, uint sessionId)
    {
        byte* respPtr = null, trailerPtr = null;
        uint respLen = 0, trailerLen = 0;
        byte respFlags = 0;
        ulong respBuf = 0, trailerBuf = 0;
        int r;
        fixed (byte* p = pair)
            r = Native.aster_call_unary(Rt, Conn, sessionId, p, (uint)pair.Length,
                &respPtr, &respLen, &respFlags, &respBuf, &trailerPtr, &trailerLen, &trailerBuf);
        if (r != 0) throw CallError(r, "aster_call_unary");
        try
        {
            byte[]? payload = respBuf != 0 || respLen > 0 ? new ReadOnlySpan<byte>(respPtr, (int)respLen).ToArray() : null;
            byte[] trailer = trailerLen > 0 ? new ReadOnlySpan<byte>(trailerPtr, (int)trailerLen).ToArray() : Array.Empty<byte>();
            return (payload, respFlags, trailer);
        }
        finally
        {
            if (respBuf != 0) Native.aster_call_buffer_release(Rt, respBuf);
            if (trailerBuf != 0) Native.aster_call_buffer_release(Rt, trailerBuf);
        }
    }

    /// <summary>A server-streaming call: one request, a stream of responses.</summary>
    public async IAsyncEnumerable<TResp> ServerStreamAsync<TReq, TResp>(string service, string method, TReq request,
        CallOptions? options = null, [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        await using var call = await StreamingCall.OpenAsync(this, service, method, options, cancellationToken).ConfigureAwait(false);
        await call.SendAsync(_codec.Encode(request), end: true, cancellationToken).ConfigureAwait(false);
        await foreach (var payload in call.ReadResponsesAsync(cancellationToken).ConfigureAwait(false))
            yield return _codec.Decode<TResp>(payload);
        call.Succeeded();
    }

    /// <summary>A client-streaming call: a stream of requests, one response.</summary>
    public async Task<TResp> ClientStreamAsync<TReq, TResp>(string service, string method, IAsyncEnumerable<TReq> requests,
        CallOptions? options = null, CancellationToken cancellationToken = default)
    {
        await using var call = await StreamingCall.OpenAsync(this, service, method, options, cancellationToken).ConfigureAwait(false);
        await call.SendAllAsync(requests, _codec, cancellationToken).ConfigureAwait(false);
        byte[]? response = null;
        int count = 0;
        await foreach (var payload in call.ReadResponsesAsync(cancellationToken).ConfigureAwait(false))
        {
            response = payload;
            count++;
        }
        if (response is null)
            throw new RpcError(StatusCode.Internal, "OK trailer with no response frame");
        if (count > 1)
            throw new RpcError(StatusCode.Internal, $"client-stream call received {count} response frames");
        call.Succeeded();
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
        await using var call = await StreamingCall.OpenAsync(this, service, method, options, cancellationToken).ConfigureAwait(false);
        using var sendCts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        var sending = call.SendAllAsync(requests, _codec, sendCts.Token);
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
        call.Succeeded();
    }

    public ValueTask DisposeAsync()
    {
        try { _connection.Close(); }
        catch (IrohException) { /* already closed */ }
        return ValueTask.CompletedTask;
    }

    internal static Exception CallError(int code, string what) => code switch
    {
        -10 => new RpcError(StatusCode.ResourceExhausted, $"{what}: stream pool full"),
        -11 => new RpcError(StatusCode.ResourceExhausted, $"{what}: QUIC stream limit reached"),
        -12 => new RpcError(StatusCode.ResourceExhausted, $"{what}: peer stream limit too low"),
        -13 => new RpcError(StatusCode.Unavailable, $"{what}: stream open failed: {IrohException.LastErrorMessage()}"),
        -14 => new RpcError(StatusCode.Unavailable, $"{what}: stream pool closed"),
        _ => new RpcError(StatusCode.Unavailable, IrohException.FromStatus(code, what).Message),
    };

    /// <summary>One streaming call on a dedicated substream (<c>aster_call_acquire_streaming</c>).</summary>
    private sealed class StreamingCall : IAsyncDisposable
    {
        private readonly RpcConnection _owner;
        private readonly ulong _call;
        private bool _succeeded;

        private StreamingCall(RpcConnection owner, ulong call)
        {
            _owner = owner;
            _call = call;
        }

        public static async Task<StreamingCall> OpenAsync(RpcConnection owner, string service, string method, CallOptions? options, CancellationToken ct)
        {
            ulong handle = await Task.Run(() =>
            {
                int r = Native.aster_call_acquire_streaming(owner.Rt, owner.Conn, out ulong h);
                if (r != 0) throw CallError(r, "aster_call_acquire_streaming");
                return h;
            }, ct).ConfigureAwait(false);
            var call = new StreamingCall(owner, handle);
            try
            {
                await call.SendFrameAsync(HeaderFrame(service, method, options), ct).ConfigureAwait(false);
                return call;
            }
            catch
            {
                await call.DisposeAsync().ConfigureAwait(false);
                throw;
            }
        }

        public Task SendAsync(byte[] payload, bool end, CancellationToken ct)
            => SendFrameAsync(AsterFraming.EncodePayloadFrame(payload, end ? AsterFraming.FlagEndStream : (byte)0), ct);

        public async Task SendAllAsync<TReq>(IAsyncEnumerable<TReq> requests, ICodec codec, CancellationToken ct)
        {
            // One message is held back so the last frame can carry END_STREAM.
            byte[]? pending = null;
            await foreach (var r in requests.WithCancellation(ct).ConfigureAwait(false))
            {
                if (pending != null) await SendAsync(pending, end: false, ct).ConfigureAwait(false);
                pending = codec.Encode(r);
            }
            if (pending != null)
                await SendAsync(pending, end: true, ct).ConfigureAwait(false);
            else
                await SendFrameAsync(AsterFraming.EncodeFrame(ReadOnlySpan<byte>.Empty, AsterFraming.FlagEndStream), ct).ConfigureAwait(false);
        }

        private Task SendFrameAsync(byte[] frame, CancellationToken ct) => Task.Run(() =>
        {
            unsafe
            {
                fixed (byte* p = frame)
                {
                    int r = Native.aster_call_send_frame(_owner.Rt, _call, p, (uint)frame.Length);
                    if (r != 0) throw CallError(r, "aster_call_send_frame");
                }
            }
        }, ct);

        /// <summary>Response payloads until the trailer; a non-OK trailer throws <see cref="RpcError"/>.</summary>
        public async IAsyncEnumerable<byte[]> ReadResponsesAsync([EnumeratorCancellation] CancellationToken ct)
        {
            while (true)
            {
                ct.ThrowIfCancellationRequested();
                var (status, payload, flags) = await Task.Run(() => Recv(RecvPollMs), ct).ConfigureAwait(false);
                if (status == 2) continue; // timeout: poll again
                if (status == 1) throw new RpcError(StatusCode.Unavailable, "stream ended before trailer");
                if ((flags & AsterFraming.FlagTrailer) != 0)
                {
                    (payload.Length == 0 ? RpcStatus.Ok : AsterEnvelope.DecodeRpcStatus(payload)).ThrowIfError();
                    yield break;
                }
                if ((flags & AsterFraming.FlagRowSchema) != 0) continue;
                yield return AsterFraming.PayloadOf(payload, flags);
            }
        }

        private unsafe (int Status, byte[] Payload, byte Flags) Recv(uint timeoutMs)
        {
            byte* ptr = null;
            uint len = 0;
            byte flags = 0;
            ulong buffer = 0;
            int r = Native.aster_call_recv_frame(_owner.Rt, _call, timeoutMs, &ptr, &len, &flags, &buffer);
            if (r < 0) throw CallError(r, "aster_call_recv_frame");
            if (r != 0) return (r, Array.Empty<byte>(), 0);
            try { return (0, len == 0 ? Array.Empty<byte>() : new ReadOnlySpan<byte>(ptr, (int)len).ToArray(), flags); }
            finally
            {
                if (buffer != 0) Native.aster_call_buffer_release(_owner.Rt, buffer);
            }
        }

        public void Succeeded() => _succeeded = true;

        public ValueTask DisposeAsync()
        {
            // Release returns the substream cleanly after the trailer; anything else drops it.
            if (_succeeded) Native.aster_call_release(_owner.Rt, _call);
            else Native.aster_call_discard(_owner.Rt, _call);
            return ValueTask.CompletedTask;
        }
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
