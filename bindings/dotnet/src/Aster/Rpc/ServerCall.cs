using System;
using System.Collections.Generic;
using System.Runtime.CompilerServices;
using System.Threading;
using System.Threading.Tasks;
using Aster.Codec;

namespace Aster.Rpc;

/// <summary>
/// One inbound call on the dispatch server (<c>aster_dispatch_*</c>): the first request payload
/// from the <c>CALL</c> event, the rest pulled by call id, and the response side. Rust frames,
/// compresses and writes the trailer; this class only passes payloads and AEv1
/// <c>RpcStatus</c> bytes. Every exit path ends the call exactly once (a no-op when Rust already
/// ended it: deadline, peer cancel, lost connection).
/// </summary>
internal sealed class ServerCall
{

    private readonly DispatchServer _dispatch;
    private readonly ulong _callId;
    private readonly byte[] _firstRequest;
    private readonly bool _firstIsEnd;
    private int _completed;

    public ServerCall(DispatchServer dispatch, ulong callId, byte[] firstRequest, bool firstIsEnd, ICodec codec, ServerCallContext context)
    {
        _dispatch = dispatch;
        _callId = callId;
        _firstRequest = firstRequest;
        _firstIsEnd = firstIsEnd;
        Codec = codec;
        Context = context;
    }

    public ICodec Codec { get; }
    public ServerCallContext Context { get; }
    public bool Completed => Volatile.Read(ref _completed) != 0;

    /// <summary>The first request payload (already decompressed by Rust).</summary>
    public byte[] FirstRequest => _firstRequest;

    /// <summary>
    /// Every request payload of a client-stream / bidi call: the first one, then the payloads
    /// pulled with <c>aster_dispatch_recv</c> until the end of the request stream. An empty first
    /// payload (a client that sent no messages) is skipped: an empty payload is never a valid
    /// message.
    /// </summary>
    public async IAsyncEnumerable<byte[]> ReadRequestsAsync([EnumeratorCancellation] CancellationToken ct = default)
    {
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(ct, Context.CancellationToken);
        var token = linked.Token;
        if (_firstRequest.Length > 0)
            yield return _firstRequest;
        if (_firstIsEnd)
            yield break;
        while (true)
        {
            token.ThrowIfCancellationRequested();
            // Buffered payloads are taken without a thread hop; only an empty queue waits, off the
            // caller's thread, in bounded aster_dispatch_recv slices that re-check the handler's
            // token, so a cancelled handler stops waiting at once and frees the waiting thread
            // within one slice. The call itself ending (cancel, deadline, lost connection, server
            // stop) ends the wait immediately.
            var status = _dispatch.Recv(_callId, 0, out byte[] payload);
            if (status == DispatchServer.RecvStatus.Timeout)
                (status, payload) = await Task.Run(() => WaitForRequest(token), CancellationToken.None).WaitAsync(token).ConfigureAwait(false);
            switch (status)
            {
                case DispatchServer.RecvStatus.EndOfStream:
                    yield break;
                case DispatchServer.RecvStatus.Gone:
                    throw new OperationCanceledException("the call is over");
                case DispatchServer.RecvStatus.Timeout:
                    token.ThrowIfCancellationRequested();
                    continue;
            }
            if (payload.Length > 0)
                yield return payload;
        }
    }

    /// <summary>Slice of one blocking <c>aster_dispatch_recv</c> wait (ms).</summary>
    internal const uint RecvSliceMs = 200;

    private (DispatchServer.RecvStatus, byte[]) WaitForRequest(CancellationToken token)
    {
        while (true)
        {
            var s = _dispatch.Recv(_callId, RecvSliceMs, out var p);
            if (s != DispatchServer.RecvStatus.Timeout || token.IsCancellationRequested)
                return (s, p);
        }
    }

    /// <summary>Sends one streaming response message; throws <see cref="OperationCanceledException"/> once the call is over.</summary>
    public async ValueTask SendAsync(byte[] payload)
    {
        if (!await _dispatch.SendAsync(_callId, payload).ConfigureAwait(false))
            throw new OperationCanceledException("the call is over (cancelled, past its deadline, or the peer went away)");
    }

    /// <summary>Sends the single response message and the OK trailer in one operation.</summary>
    public ValueTask CompleteUnaryAsync(byte[] payload)
    {
        if (Interlocked.Exchange(ref _completed, 1) != 0)
            throw new InvalidOperationException("call already completed");
        return Ignore(_dispatch.RespondAsync(_callId, payload, null));
    }

    /// <summary>Ends the call with <paramref name="status"/> (no-op if it already ended).</summary>
    public ValueTask CompleteAsync(RpcStatus status)
    {
        if (Interlocked.Exchange(ref _completed, 1) != 0)
            return ValueTask.CompletedTask;
        byte[]? trailer = status.Code == Interceptors.StatusCode.Ok ? null : AsterEnvelope.Encode(status);
        return Ignore(Context.Pattern is MethodPattern.Unary or MethodPattern.ClientStream
            ? _dispatch.RespondAsync(_callId, null, trailer)
            : _dispatch.FinishAsync(_callId, trailer));
    }

    /// <summary>Ends the call with <c>INTERNAL "handler dropped call"</c> unless it already ended.</summary>
    public void Abandon()
    {
        if (Interlocked.Exchange(ref _completed, 1) == 0)
            _dispatch.Abandon(_callId);
    }

    // A false outcome means the call already ended (Rust answered or nobody is listening).
    private static ValueTask Ignore(ValueTask<bool> op)
    {
        if (op.IsCompletedSuccessfully) return ValueTask.CompletedTask;
        return Await(op);

        static async ValueTask Await(ValueTask<bool> op) => await op.ConfigureAwait(false);
    }
}
