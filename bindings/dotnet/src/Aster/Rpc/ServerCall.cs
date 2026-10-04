using System;
using System.Collections.Generic;
using System.Runtime.CompilerServices;
using System.Threading;
using System.Threading.Tasks;
using Aster.Codec;

namespace Aster.Rpc;

/// <summary>
/// One inbound call on the reactor: decoded header, the inline first request frame, and the
/// response side. Every exit path ends the call with exactly one AEv1 <c>RpcStatus</c>
/// TRAILER frame.
/// </summary>
internal sealed class ServerCall
{
    private const uint RecvPollMs = 100;

    private readonly Reactor _reactor;
    private readonly ulong _callId;
    private readonly byte[] _firstRequest;
    private readonly byte _firstFlags;
    private int _completed;

    public ServerCall(Reactor reactor, ulong callId, byte[] firstRequest, byte firstFlags, ICodec codec, ServerCallContext context)
    {
        _reactor = reactor;
        _callId = callId;
        _firstRequest = firstRequest;
        _firstFlags = firstFlags;
        Codec = codec;
        Context = context;
    }

    public ICodec Codec { get; }
    public ServerCallContext Context { get; }
    public bool Completed => Volatile.Read(ref _completed) != 0;

    /// <summary>The logical (decompressed) first request payload.</summary>
    public byte[] FirstRequest => AsterFraming.PayloadOf(_firstRequest, _firstFlags);

    /// <summary>
    /// Every request payload of a client-stream / bidi call: the inline first frame, then the
    /// frames pulled from the reactor until END_STREAM. An empty inline frame (a client that
    /// sent no messages) is skipped: an empty payload is never a valid message.
    /// </summary>
    public async IAsyncEnumerable<byte[]> ReadRequestsAsync([EnumeratorCancellation] CancellationToken ct = default)
    {
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(ct, Context.CancellationToken);
        var token = linked.Token;
        if (_firstRequest.Length > 0)
            yield return FirstRequest;
        if ((_firstFlags & AsterFraming.FlagEndStream) != 0)
            yield break;
        while (true)
        {
            token.ThrowIfCancellationRequested();
            // aster_reactor_recv_frame blocks for up to RecvPollMs: keep it off the caller's thread.
            var (status, payload, flags) = await Task.Run(() =>
            {
                var s = _reactor.RecvFrame(_callId, RecvPollMs, out var p, out var f);
                return (s, p, f);
            }, token).ConfigureAwait(false);
            switch (status)
            {
                case Reactor.RecvStatus.EndOfStream:
                    yield break;
                case Reactor.RecvStatus.Timeout:
                    continue;
            }
            if (payload.Length > 0)
                yield return AsterFraming.PayloadOf(payload, flags);
            if ((flags & AsterFraming.FlagEndStream) != 0)
                yield break;
        }
    }

    /// <summary>Sends one streaming response message.</summary>
    public Task SendAsync(byte[] payload)
    {
        if (_reactor.IsCancelled(_callId))
            throw new OperationCanceledException("call cancelled by the peer");
        return _reactor.SubmitFrameAsync(_callId, AsterFraming.EncodePayloadFrame(payload, 0), Context.CancellationToken);
    }

    /// <summary>Sends the single response message and the OK trailer in one submit.</summary>
    public Task CompleteUnaryAsync(byte[] payload)
    {
        if (Interlocked.Exchange(ref _completed, 1) != 0)
            throw new InvalidOperationException("call already completed");
        return _reactor.SubmitAsync(_callId, ReactorResponse.Of(
            AsterFraming.EncodePayloadFrame(payload, 0),
            TrailerFrame(RpcStatus.Ok)));
    }

    /// <summary>Ends the call with <paramref name="status"/> (no-op if it already ended).</summary>
    public Task CompleteAsync(RpcStatus status)
    {
        if (Interlocked.Exchange(ref _completed, 1) != 0)
            return Task.CompletedTask;
        return _reactor.SubmitTrailerAsync(_callId, TrailerFrame(status));
    }

    /// <summary>An AEv1 RpcStatus TRAILER frame.</summary>
    public static byte[] TrailerFrame(RpcStatus status)
        => AsterFraming.EncodeFrame(AsterEnvelope.Encode(status), AsterFraming.FlagTrailer);
}
