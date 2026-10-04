using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Upcall;

namespace Aster.Expose;

/// <summary>HTTP version of an exposed request (<c>C4</c> <c>version</c>).</summary>
public enum HttpProtocolVersion : byte
{
    Http09 = 0,
    Http10 = 1,
    Http11 = 2,
    Http2 = 3,
    Http3 = 4,
}

/// <summary>One request to an exposed HTTP service (kind 49).</summary>
public sealed class ExposeHttpRequest
{
    internal ExposeHttpRequest(ExposeHttpRequestWire wire, Stream body, UpcallContext upcall)
    {
        ServiceId = wire.ServiceId;
        PeerId = wire.PeerId;
        StreamId = wire.StreamId;
        Version = (HttpProtocolVersion)wire.Version;
        Method = wire.Method;
        Uri = wire.Uri;
        Headers = wire.Headers;
        Body = body;
        _upcall = upcall;
    }

    private readonly UpcallContext _upcall;

    public string ServiceId { get; }

    /// <summary>The transport-authenticated peer; null off the relay.</summary>
    public string? PeerId { get; }

    /// <summary>The exchange's body handle id.</summary>
    public ulong StreamId { get; }
    public HttpProtocolVersion Version { get; }
    public string Method { get; }
    public string Uri { get; }
    public IReadOnlyList<HttpHeader> Headers { get; }

    /// <summary>The request body: inline bytes, or a pull-based stream over the exchange (end at once when there is no body).</summary>
    public Stream Body { get; }

    /// <summary>Cancelled when the consumer abandons the request before the head, the head is not answered in time, or the table shuts down.</summary>
    public CancellationToken Cancellation => _upcall.Cancellation;

    /// <summary>Why <see cref="Cancellation"/> fired.</summary>
    public UpcallCancelReason CancelReason => _upcall.CancelReason;

    /// <summary>The first header named <paramref name="name"/> (case-insensitive) as text, or null.</summary>
    public string? Header(string name)
        => Headers.FirstOrDefault(h => string.Equals(h.Name, name, StringComparison.OrdinalIgnoreCase))?.ValueText;
}

/// <summary>The response of an exposed HTTP service: an inline body, or a streamed one.</summary>
public sealed class ExposeHttpResponse
{
    private ExposeHttpResponse(ushort status, IReadOnlyList<HttpHeader> headers, byte[]? body, IAsyncEnumerable<ReadOnlyMemory<byte>>? stream, IReadOnlyList<HttpHeader>? trailers)
    {
        if (status is < 100 or > 999) throw new ArgumentOutOfRangeException(nameof(status), "HTTP status must be 100..=999");
        foreach (var h in headers)
            if (h.Name.Length == 0 || h.Name.Any(char.IsAsciiLetterUpper))
                throw new ArgumentException($"header name \"{h.Name}\" must be non-empty and lowercase", nameof(headers));
        Status = status;
        Headers = headers;
        InlineBody = body;
        StreamBody = stream;
        Trailers = trailers;
    }

    public ushort Status { get; }
    public IReadOnlyList<HttpHeader> Headers { get; }
    public byte[]? InlineBody { get; }
    public IAsyncEnumerable<ReadOnlyMemory<byte>>? StreamBody { get; }

    /// <summary>Sent with the end of a streamed body (the relay hop does not carry them yet).</summary>
    public IReadOnlyList<HttpHeader>? Trailers { get; }

    /// <summary>A complete response in one round trip.</summary>
    public static ExposeHttpResponse Inline(ushort status, byte[]? body = null, IReadOnlyList<HttpHeader>? headers = null)
        => new(status, headers ?? Array.Empty<HttpHeader>(), body ?? Array.Empty<byte>(), null, null);

    public static ExposeHttpResponse Text(ushort status, string body, string contentType = "text/plain; charset=utf-8")
        => Inline(status, Encoding.UTF8.GetBytes(body), new[] { new HttpHeader("content-type", contentType) });

    /// <summary>
    /// A streamed response: the head goes out at once, then each chunk is written as the sequence
    /// yields it (with backpressure), then the body is finished (with <paramref name="trailers"/>). If
    /// the sequence throws, the body is aborted and the consumer sees a failed response.
    /// </summary>
    public static ExposeHttpResponse Streaming(ushort status, IAsyncEnumerable<ReadOnlyMemory<byte>> body,
        IReadOnlyList<HttpHeader>? headers = null, IReadOnlyList<HttpHeader>? trailers = null)
        => new(status, headers ?? Array.Empty<HttpHeader>(), null, body, trailers);
}

/// <summary>Handles one exposed HTTP request. An exception answers <c>500</c> (before the head) or aborts the body (after it).</summary>
public delegate ValueTask<ExposeHttpResponse> ExposeHttpHandler(ExposeHttpRequest request);

/// <summary>Options of a foreign HTTP service (<c>aster_http_foreign_options_t</c>). Zero = the default.</summary>
public sealed record HttpForeignOptions
{
    /// <summary>A request whose <c>content-length</c> is at most this travels inline in the upcall (default 64 KiB).</summary>
    public ulong InlineRequestMax { get; init; }

    /// <summary>Cap on the request body: a declared length over it is answered 413; a streamed body that grows past it fails the read with <see cref="HttpBodyStatus.TooLarge"/>. 0 = unbounded.</summary>
    public ulong MaxRequestBody { get; init; }

    /// <summary>Cap on the response body: an inline body over it becomes 502, a streamed one is aborted by the write that crosses it. 0 = unbounded.</summary>
    public ulong MaxResponseBody { get; init; }

    /// <summary>Largest single body write and most one read returns (default 1 MiB). Streamed responses are written in chunks of at most <see cref="WriteChunkSize"/>.</summary>
    public ulong MaxChunk { get; init; }

    /// <summary>Response chunks queued before a write waits (default 8).</summary>
    public uint ResponseBuffer { get; init; }

    /// <summary>Size at which <see cref="ExposeHttpResponse.Streaming"/> chunks are split (default 64 KiB; must not exceed <see cref="MaxChunk"/> when that is set).</summary>
    public int WriteChunkSize { get; init; } = 64 * 1024;

    internal unsafe HttpForeignOptionsNative ToNative()
    {
        HttpForeignOptionsNative o = default;
        Native.Check(Native.aster_http_foreign_options_init(&o), "aster_http_foreign_options_init");
        o.inline_request_max = InlineRequestMax;
        o.max_request_body = MaxRequestBody;
        o.max_response_body = MaxResponseBody;
        o.max_chunk = MaxChunk;
        o.response_buffer = ResponseBuffer;
        return o;
    }
}

/// <summary>
/// Routes kind-49 upcalls of one table to the exposed HTTP services registered on it. The table's
/// kind-49 handler is installed on first use; a request for a service id with no route answers
/// <c>ERROR</c> (500).
/// </summary>
internal sealed class HttpRouter
{
    private static readonly ConditionalWeakTable<Upcalls, HttpRouter> Routers = new();

    private readonly ConcurrentDictionary<string, Route> _routes = new();

    private sealed record Route(HttpBodies Bodies, ExposeHttpHandler Handler, int ChunkSize);

    public static HttpRouter For(Upcalls upcalls)
    {
        lock (Routers)
        {
            if (Routers.TryGetValue(upcalls, out var existing)) return existing;
            var router = new HttpRouter();
            upcalls.Handle(UpcallKind.ExposeHttp, router.HandleAsync);
            Routers.Add(upcalls, router);
            return router;
        }
    }

    public void Add(string serviceId, HttpBodies bodies, ExposeHttpHandler handler, int chunkSize)
        => _routes[serviceId] = new Route(bodies, handler, Math.Max(1, chunkSize));

    public void Remove(string serviceId) => _routes.TryRemove(serviceId, out _);

    private async ValueTask<byte[]?> HandleAsync(byte[] payload, UpcallContext ctx)
    {
        var wire = Ucv1.DecodeExposeHttpRequest(payload);
        if (!_routes.TryGetValue(wire.ServiceId, out var route))
            throw new InvalidOperationException($"no HTTP handler registered for service \"{wire.ServiceId}\"");

        Stream body = wire.Body is HttpBody.Inline inline
            ? new MemoryStream(inline.Body, writable: false)
            : route.Bodies.OpenRequestBody(wire.StreamId);
        using var bodyScope = body;
        var request = new ExposeHttpRequest(wire, body, ctx);

        ExposeHttpResponse response = await route.Handler(request).ConfigureAwait(false)
            ?? throw new InvalidOperationException("ExposeHttpHandler returned null");

        if (response.StreamBody == null)
        {
            return Ucv1.EncodeExposeHttpResponse(new ExposeHttpResponseWire(response.Status, response.Headers, new HttpBody.Inline(response.InlineBody ?? Array.Empty<byte>())));
        }

        // Head first, then the body through the exchange's body handle.
        byte[] head = Ucv1.EncodeExposeHttpResponse(new ExposeHttpResponseWire(response.Status, response.Headers, HttpBody.Stream));
        if (!ctx.TryRespond(head))
            return null; // Rust no longer waits (timeout / abandoned): the exchange is gone
        await PumpAsync(route, wire.StreamId, response, ctx.Cancellation).ConfigureAwait(false);
        return null;
    }

    private static async Task PumpAsync(Route route, ulong streamId, ExposeHttpResponse response, CancellationToken ct)
    {
        try
        {
            await foreach (ReadOnlyMemory<byte> chunk in response.StreamBody!.WithCancellation(ct).ConfigureAwait(false))
            {
                for (int off = 0; off < chunk.Length; off += route.ChunkSize)
                    await route.Bodies.WriteAsync(streamId, chunk.Slice(off, Math.Min(route.ChunkSize, chunk.Length - off)), ct).ConfigureAwait(false);
            }
            route.Bodies.Finish(streamId, response.Trailers);
        }
        catch (HttpBodyException e) when (e.IsCancelled || e.BodyStatus is HttpBodyStatus.UnknownStream or HttpBodyStatus.Finished or HttpBodyStatus.TooLarge)
        {
            // The peer is gone, or the relay already ended / aborted the exchange: nothing left to tell it.
        }
        catch (Exception e)
        {
            // Includes cancellation by shutdown: the consumer must see a failed response, not a truncated one.
            try { route.Bodies.Abort(streamId, e.Message); }
            catch (Exception) { /* exchange already over */ }
        }
    }
}
