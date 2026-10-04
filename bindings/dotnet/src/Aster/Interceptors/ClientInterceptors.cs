using System.Runtime.CompilerServices;

namespace Aster.Interceptors;

/// <summary>
/// The client-side interceptor stack of an <see cref="AsterClient"/> / <see cref="RpcConnection"/>,
/// with Python's semantics (<c>aster/client.py</c>):
/// <list type="bullet">
/// <item><b>Hooks.</b> Every interceptor's <c>OnRequest</c> / <c>OnResponse</c> run in list
/// order, <c>OnError</c> in reverse; <c>OnError</c> returning <c>null</c> suppresses the error
/// (the call yields <c>default</c> / ends the stream). Request and response hooks may replace
/// the message but not change its type. Changes to <see cref="CallContext.Metadata"/> are sent
/// with the call.</item>
/// <item><b>Unary and client-streaming</b> calls run the retry loop: per attempt
/// <see cref="CircuitBreakerInterceptor.BeforeCall"/> (an open circuit throws
/// <c>Unavailable</c> straight away: no hooks, no retry), the request hooks, the call,
/// the response hooks, then <c>RecordSuccess</c>; on failure <c>RecordFailure</c>, the error
/// hooks, and a retry if a <see cref="RetryInterceptor"/> allows it (idempotent method,
/// retryable code, attempts left) after its back-off. The call is retried with the original
/// request; a client-stream's request sequence is enumerated again, so it must be replayable.</item>
/// <item><b>Deadlines.</b> <c>CallOptions.Deadline</c> becomes the context's absolute deadline.
/// With a <see cref="DeadlineInterceptor"/>, every attempt (and the back-off) is bounded by the
/// time left, which is also what the native call is given (Rust enforces it), and an expired
/// deadline is refused before sending. Without one the full deadline applies to each attempt.</item>
/// <item><b>Server-streaming and bidi</b> calls get the hooks only (per message, and the error
/// hooks on a failed stream) -- no retry, no breaker.</item>
/// <item><c>OperationCanceledException</c> from the caller's token is never an RPC failure: it
/// skips the breaker, the error hooks and retries.</item>
/// </list>
/// Hooks are synchronous (<see cref="IInterceptor"/>, shared with the server side).
/// </summary>
internal sealed class ClientInterceptors
{
    public static readonly ClientInterceptors None = new([]);

    private readonly IInterceptor[] _all;
    private readonly RetryInterceptor[] _retries;
    private readonly CircuitBreakerInterceptor[] _breakers;
    private readonly DeadlineInterceptor[] _deadlines;
    private readonly int _maxAttempts;

    public ClientInterceptors(IEnumerable<IInterceptor> interceptors)
    {
        _all = [.. interceptors];
        _retries = [.. _all.OfType<RetryInterceptor>()];
        _breakers = [.. _all.OfType<CircuitBreakerInterceptor>()];
        _deadlines = [.. _all.OfType<DeadlineInterceptor>()];
        _maxAttempts = _retries.Length == 0 ? 1 : Math.Max(1, _retries.Max(r => r.MaxAttempts));
    }

    public IReadOnlyList<IInterceptor> All => _all;

    public bool IsEmpty => _all.Length == 0;

    public CallContext NewContext(string service, string method, CallOptions? options, string pattern, string? peer)
    {
        var ctx = new CallContext
        {
            Service = service,
            Method = method,
            Pattern = pattern,
            IsStreaming = pattern != "unary",
            Idempotent = options?.Idempotent ?? false,
            SessionId = options is { SessionId: not 0 } ? options.SessionId.ToString() : null,
            Peer = peer,
        };
        if (options?.Metadata is { } md)
            foreach (var kv in md) ctx.Metadata[kv.Key] = kv.Value;
        if (options?.Deadline is { } d && d > TimeSpan.Zero)
            ctx.Deadline = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() / 1000.0 + d.TotalSeconds;
        return ctx;
    }

    /// <summary>The options of one attempt: the context's metadata and, with a deadline interceptor, the time left.</summary>
    private CallOptions AttemptOptions(CallContext ctx, CallOptions? options)
    {
        var o = options ?? new CallOptions();
        if (ctx.Metadata.Count > 0 || o.Metadata is not null)
            o = o with { Metadata = [.. ctx.Metadata] };
        if (_deadlines.Length > 0 && ctx.RemainingSeconds is { } left)
        {
            if (left <= 0.0) throw new RpcError(StatusCode.DeadlineExceeded, "deadline exceeded");
            o = o with { Deadline = TimeSpan.FromSeconds(left) };
        }
        return o;
    }

    private void CheckDeadlines(CallContext ctx)
    {
        foreach (var d in _deadlines) d.CheckDeadline(ctx);
    }

    /// <summary>
    /// The unary / client-stream call loop. <paramref name="hookRequest"/> is false for a client
    /// stream, whose request hooks run per message in <see cref="HookRequests{T}"/>.
    /// </summary>
    public async Task<TResp> RunAsync<TReq, TResp>(CallContext ctx, TReq request, CallOptions? options, bool hookRequest,
        Func<TReq, CallOptions, Task<TResp>> invoke, CancellationToken ct)
    {
        for (int attempt = 1; attempt <= _maxAttempts; attempt++)
        {
            ct.ThrowIfCancellationRequested();
            ctx.Attempt = attempt;
            foreach (var b in _breakers) b.BeforeCall();

            try
            {
                TReq current = request;
                if (hookRequest) current = (TReq)InterceptorChain.ApplyRequest(_all, ctx, request!);
                else CheckDeadlines(ctx);
                TResp response = await invoke(current, AttemptOptions(ctx, options)).ConfigureAwait(false);
                response = (TResp)InterceptorChain.ApplyResponse(_all, ctx, response!);
                foreach (var b in _breakers) b.RecordSuccess();
                return response;
            }
            catch (Exception ex) when (!IsCallerCancellation(ex, ct))
            {
                RpcError error = Normalize(ex);
                foreach (var b in _breakers) b.RecordFailure(error);
                RpcError? handled = InterceptorChain.ApplyError(_all, ctx, error);
                if (handled is null) return default!;

                double delay = -1;
                foreach (var r in _retries)
                    if (attempt < r.MaxAttempts && r.ShouldRetry(ctx, handled))
                        delay = Math.Max(delay, r.BackoffSeconds(attempt));
                if (delay < 0)
                {
                    if (ReferenceEquals(handled, ex)) throw;
                    throw handled;
                }
                if (_deadlines.Length > 0 && ctx.RemainingSeconds is { } left)
                {
                    if (left <= 0.0) throw new RpcError(StatusCode.DeadlineExceeded, "deadline exceeded");
                    delay = Math.Min(delay, left);
                }
                await Task.Delay(TimeSpan.FromSeconds(delay), ct).ConfigureAwait(false);
            }
        }
        throw new RpcError(StatusCode.Internal, "call failed");
    }

    /// <summary>
    /// A server-stream's responses: the request hooks run first (a failure there is not passed to
    /// the error hooks), then each response through the response hooks; a failure while reading goes
    /// to the error hooks, which may suppress it (the stream just ends).
    /// </summary>
    public async IAsyncEnumerable<TResp> ServerStreamAsync<TReq, TResp>(CallContext ctx, TReq request, CallOptions? options,
        Func<TReq, CallOptions, CancellationToken, IAsyncEnumerable<TResp>> open, [EnumeratorCancellation] CancellationToken ct)
    {
        var current = (TReq)InterceptorChain.ApplyRequest(_all, ctx, request!);
        await foreach (var item in Guard(open(current, AttemptOptions(ctx, options), ct), ctx, ct).ConfigureAwait(false))
            yield return item;
    }

    /// <summary>Wraps a response stream with the response and error hooks.</summary>
    public async IAsyncEnumerable<TResp> Guard<TResp>(IAsyncEnumerable<TResp> source, CallContext ctx, [EnumeratorCancellation] CancellationToken ct)
    {
        await using var e = source.GetAsyncEnumerator(ct);
        while (true)
        {
            TResp item;
            try
            {
                if (!await e.MoveNextAsync().ConfigureAwait(false)) yield break;
                item = (TResp)InterceptorChain.ApplyResponse(_all, ctx, e.Current!);
            }
            catch (Exception ex) when (!IsCallerCancellation(ex, ct))
            {
                RpcError? handled = InterceptorChain.ApplyError(_all, ctx, Normalize(ex));
                if (handled is null) yield break;
                if (ReferenceEquals(handled, ex)) throw;
                throw handled;
            }
            yield return item;
        }
    }

    /// <summary>The call's request messages through the request hooks (client-stream and bidi).</summary>
    public async IAsyncEnumerable<T> HookRequests<T>(IAsyncEnumerable<T> source, CallContext ctx, [EnumeratorCancellation] CancellationToken ct = default)
    {
        await foreach (var item in source.WithCancellation(ct).ConfigureAwait(false))
            yield return (T)InterceptorChain.ApplyRequest(_all, ctx, item!);
    }

    /// <summary>Per-call setup for bidi: deadline check, then options carrying the context's metadata and time left.</summary>
    public CallOptions StartStreaming(CallContext ctx, CallOptions? options)
    {
        CheckDeadlines(ctx);
        return AttemptOptions(ctx, options);
    }

    private static bool IsCallerCancellation(Exception ex, CancellationToken ct)
        => ex is OperationCanceledException && ct.IsCancellationRequested;

    /// <summary>An <see cref="RpcError"/> for any failure (timeouts are <c>DeadlineExceeded</c>, the rest <c>Unknown</c>).</summary>
    internal static RpcError Normalize(Exception ex) => ex switch
    {
        RpcError e => e,
        OperationCanceledException or TimeoutException => new RpcError(StatusCode.DeadlineExceeded, "deadline exceeded"),
        _ => new RpcError(StatusCode.Unknown, ex.Message),
    };
}
