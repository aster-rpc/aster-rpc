using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.CompilerServices;
using System.Runtime.ExceptionServices;
using System.Threading;
using System.Threading.Tasks;
using Aster.Upcall;

namespace Aster.Expose;

public static partial class ExposeClient
{
    internal static async IAsyncEnumerable<ReadOnlyMemory<byte>> FromStream(Stream s, [EnumeratorCancellation] CancellationToken ct = default)
    {
        byte[] buf = new byte[64 * 1024];
        int n;
        while ((n = await s.ReadAsync(buf, ct).ConfigureAwait(false)) > 0)
            yield return buf.AsMemory(0, n); // consumed (copied by the native write) before the next read reuses buf
    }

    /// <summary>Streaming relay with a <see cref="Stream"/> request body (null = no body).</summary>
    public static Task<ExposeStreamResponse> RelayStreamAsync(Connection connection, string serviceId, string method, string uri,
        IReadOnlyList<HttpHeader>? headers, Stream? body, ExposeRelayOptions? options = null, CancellationToken cancellationToken = default)
        => RelayStreamAsync(connection, serviceId, method, uri, headers, body == null ? null : FromStream(body), options, cancellationToken);

    // `&` on locals is not allowed in async methods (CS9123); the native call lives in this synchronous helper.
    private static unsafe (ulong Upload, ulong Response) StartRelay(Runtime rt, Connection connection, byte[] c4, ExposeRelayOptions? options)
    {
        using var scope = new NativeScope();
        HttpRelayOptionsNative o = default;
        Native.Check(Native.aster_http_relay_options_init(&o), "aster_http_relay_options_init");
        if (options != null) { o.max_chunk = options.MaxChunk; o.upload_capacity = options.UploadCapacity; }
        ulong up = 0, resp = 0;
        Native.Check(Native.aster_expose_relay_stream(rt.Handle, connection.Handle.Value, scope.Bytes(c4), &o, &up, &resp), "aster_expose_relay_stream");
        return (up, resp);
    }

    /// <summary>
    /// Starts a <b>streaming</b> HTTP exchange with <paramref name="serviceId"/> exposed by the peer on
    /// <paramref name="connection"/> and returns as soon as the response head is in (full duplex: the head,
    /// e.g. an early 413, can arrive while the upload is still running, and the upload carries on in the
    /// background). <paramref name="body"/> (null = empty) is pulled with backpressure and finished at its end.
    ///
    /// <para>Failures before the head throw: the body source's own exception if it failed (the request is
    /// reset), otherwise an <see cref="ExposeRelayException"/> with a stable code (<c>Denied</c> maps to
    /// <see cref="Status.PermissionDenied"/>, <c>UnknownService</c> to <see cref="Status.NotFound"/>, ...).
    /// <paramref name="cancellationToken"/> applies until the head; afterwards cancel by disposing the response.
    /// A failure of the upload after the head is reported through <see cref="ExposeStreamResponse.Upload"/>.</para>
    /// </summary>
    public static async Task<ExposeStreamResponse> RelayStreamAsync(Connection connection, string serviceId, string method, string uri,
        IReadOnlyList<HttpHeader>? headers = null, IAsyncEnumerable<ReadOnlyMemory<byte>>? body = null, ExposeRelayOptions? options = null,
        CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(connection);
        byte[] c4 = Ucv1.EncodeExposeHttpRequest(new ExposeHttpRequestWire(serviceId, null, 0, (byte)HttpProtocolVersion.Http11, method, uri,
            headers ?? Array.Empty<HttpHeader>(), HttpBody.Stream));
        var rt = connection.Runtime;
        (ulong upload, ulong response) = StartRelay(rt, connection, c4, options);
        var relay = new ExposeStreamResponse(rt, upload, response, options?.MaxChunk ?? 0, body);
        try
        {
            await relay.AwaitHeadAsync(cancellationToken).ConfigureAwait(false);
            return relay;
        }
        catch (ExposeRelayException) when (relay.SourceError is { } src)
        {
            await relay.AbandonAsync().ConfigureAwait(false);
            ExceptionDispatchInfo.Capture(src).Throw();
            throw; // unreachable
        }
        catch
        {
            await relay.AbandonAsync().ConfigureAwait(false);
            throw;
        }
    }
}
