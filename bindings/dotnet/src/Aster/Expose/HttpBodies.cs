using System;
using System.Collections.Generic;
using System.IO;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Upcall;

namespace Aster.Expose;

/// <summary>
/// Result code of an <c>aster_http_body_*</c> call (<c>aster_http_body_status_t</c>): 1..9 are the
/// stable <c>BodyError</c> codes, the negatives are argument / handle errors.
/// </summary>
public enum HttpBodyStatus
{
    Ok = 0,
    /// <summary>No open exchange has this stream id: never issued, or already over.</summary>
    UnknownStream = 1,
    /// <summary>The peer went away or the request was abandoned: stop producing.</summary>
    Cancelled = 2,
    /// <summary>A body went over its cap (<c>max_request_body</c> for a read: answer 413; <c>max_response_body</c> for a write: the response was aborted).</summary>
    TooLarge = 3,
    /// <summary>One write was larger than <c>max_chunk</c>; split it.</summary>
    ChunkTooLarge = 4,
    /// <summary>The response body was already finished or aborted (or it was inline).</summary>
    Finished = 5,
    /// <summary>A blocking call timed out; nothing was consumed or queued.</summary>
    Timeout = 6,
    /// <summary>Reading the request body off the wire failed.</summary>
    Read = 7,
    /// <summary><c>finish</c> trailers were not a valid <c>C6</c> message.</summary>
    InvalidTrailers = 8,
    /// <summary>A blocking call was made on a tokio runtime thread.</summary>
    BlockingOnRuntime = 9,
    InvalidArgument = -1,
    UnknownHandle = -2,
}

/// <summary>An <c>aster_http_body_*</c> call failed. <see cref="HttpBodyStatus.Cancelled"/> means the peer is gone: stop producing.</summary>
public sealed class HttpBodyException : IOException
{
    public HttpBodyStatus BodyStatus { get; }

    public HttpBodyException(HttpBodyStatus status, string what, string? detail)
        : base(string.IsNullOrEmpty(detail) ? $"{what}: {status} ({(int)status})" : $"{what}: {status} ({(int)status}): {detail}")
    {
        BodyStatus = status;
    }

    /// <summary>True when the exchange is over because the peer left or the request was abandoned.</summary>
    public bool IsCancelled => BodyStatus == HttpBodyStatus.Cancelled;
}

/// <summary>
/// The body-handle table of foreign HTTP exchanges (<c>aster_http_bodies_t</c>, C ABI 4.2): request
/// bodies are pulled and response bodies pushed by the request's <c>stream_id</c>. One per binding runtime.
///
/// <para>The native calls are blocking-with-timeout. Async methods here never block a thread-pool
/// thread: they try once without waiting (timeout 0) and, while nothing is ready, re-poll after a
/// short growing delay (1 ms up to 8 ms), honouring the cancellation token. Backpressure is
/// preserved: Rust reads the QUIC stream only when <see cref="ReadAsync"/> asks, and a full response
/// buffer makes <see cref="WriteAsync"/> wait.</para>
/// </summary>
public sealed class HttpBodies : IDisposable
{
    private const int MaxPollDelayMs = 8;

    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private int _disposed;

    private HttpBodies(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    public static HttpBodies Create(Runtime runtime)
    {
        ArgumentNullException.ThrowIfNull(runtime);
        Native.Check(Native.aster_http_bodies_new(runtime.Handle, out ulong h), "aster_http_bodies_new");
        return new HttpBodies(runtime, h);
    }

    internal ulong NativeHandle => _handle;
    internal Runtime Runtime => _runtime;

    /// <summary>Open exchanges (for leak checks and tests).</summary>
    public ulong OpenCount
    {
        get
        {
            ThrowIfDisposed();
            Native.Check(Native.aster_http_bodies_open_count(_runtime.Handle, _handle, out ulong n), "aster_http_bodies_open_count");
            return n;
        }
    }

    private static async ValueTask Backoff(int attempt, CancellationToken ct)
    {
        int ms = Math.Min(1 << Math.Min(attempt, 3), MaxPollDelayMs);
        await Task.Delay(ms, ct).ConfigureAwait(false);
    }

    private HttpBodyException Fail(int rc, string what)
    {
        string? detail = IrohException.LastErrorMessage();
        return new HttpBodyException((HttpBodyStatus)rc, what, detail);
    }

    /// <summary>
    /// Reads up to <c>buffer.Length</c> bytes (at most the options' <c>max_chunk</c>) of the request
    /// body of exchange <paramref name="streamId"/>. Returns 0 at the end of the body; a short read is normal.
    /// </summary>
    public async ValueTask<int> ReadAsync(ulong streamId, Memory<byte> buffer, CancellationToken cancellationToken = default)
    {
        if (buffer.IsEmpty) return 0;
        for (int attempt = 0; ; attempt++)
        {
            ThrowIfDisposed();
            cancellationToken.ThrowIfCancellationRequested();
            int rc = ReadOnce(streamId, buffer.Span, out int n, out bool eof);
            if (rc == (int)HttpBodyStatus.Ok)
            {
                if (eof) return 0;
                if (n > 0) return n;
            }
            else if (rc != (int)HttpBodyStatus.Timeout)
            {
                throw Fail(rc, "aster_http_body_read");
            }
            await Backoff(attempt, cancellationToken).ConfigureAwait(false);
        }
    }

    private unsafe int ReadOnce(ulong streamId, Span<byte> buf, out int n, out bool eof)
    {
        UIntPtr len = UIntPtr.Zero;
        uint e = 0;
        int rc;
        fixed (byte* p = buf)
            rc = Native.aster_http_body_read(_runtime.Handle, _handle, streamId, p, (UIntPtr)buf.Length, 0, &len, &e);
        n = (int)(nuint)len;
        eof = e != 0;
        return rc;
    }

    /// <summary>
    /// Queues one response-body chunk (copied; at most the options' <c>max_chunk</c> bytes), waiting
    /// while the buffer is full. Throws <see cref="HttpBodyException"/> with
    /// <see cref="HttpBodyException.IsCancelled"/> when the peer is gone. Send the head (body mode
    /// Stream) first; writes made before it queue up to <c>response_buffer</c> chunks.
    /// </summary>
    public async ValueTask WriteAsync(ulong streamId, ReadOnlyMemory<byte> chunk, CancellationToken cancellationToken = default)
    {
        for (int attempt = 0; ; attempt++)
        {
            ThrowIfDisposed();
            cancellationToken.ThrowIfCancellationRequested();
            if (TryWrite(streamId, chunk.Span)) return;
            await Backoff(attempt, cancellationToken).ConfigureAwait(false);
        }
    }

    /// <summary>Queues one chunk if there is room now; false = the buffer is full (nothing queued). Never blocks.</summary>
    public unsafe bool TryWrite(ulong streamId, ReadOnlySpan<byte> chunk)
    {
        ThrowIfDisposed();
        uint queued = 0;
        int rc;
        fixed (byte* p = chunk)
            rc = Native.aster_http_body_try_write(_runtime.Handle, _handle, streamId, p, (UIntPtr)chunk.Length, &queued);
        if (rc != (int)HttpBodyStatus.Ok) throw Fail(rc, "aster_http_body_try_write");
        return queued != 0;
    }

    /// <summary>
    /// Ends the response body cleanly, with optional trailers (queued chunks are still delivered; the
    /// relay hop does not carry trailers yet, an in-process consumer sees them). Never blocks.
    /// </summary>
    public unsafe void Finish(ulong streamId, IReadOnlyList<HttpHeader>? trailers = null)
    {
        ThrowIfDisposed();
        byte[]? c6 = trailers is { Count: > 0 } ? Ucv1.EncodeExposeHttpTrailers(trailers) : null;
        int rc;
        fixed (byte* p = c6)
            rc = Native.aster_http_body_finish(_runtime.Handle, _handle, streamId, c6 == null ? null : p, (UIntPtr)(c6?.Length ?? 0));
        if (rc != (int)HttpBodyStatus.Ok) throw Fail(rc, "aster_http_body_finish");
    }

    /// <summary>
    /// Ends the response body with an error: queued chunks are discarded and the relay resets the
    /// stream, so the consumer sees a failed response, never a silently truncated one.
    /// <paramref name="message"/> is logged, not sent. Never blocks.
    /// </summary>
    public unsafe void Abort(ulong streamId, string? message = null)
    {
        ThrowIfDisposed();
        byte[] m = string.IsNullOrEmpty(message) ? Array.Empty<byte>() : Encoding.UTF8.GetBytes(message);
        int rc;
        fixed (byte* p = m)
            rc = Native.aster_http_body_abort(_runtime.Handle, _handle, streamId, m.Length == 0 ? null : p, (UIntPtr)m.Length);
        if (rc != (int)HttpBodyStatus.Ok) throw Fail(rc, "aster_http_body_abort");
    }

    /// <summary>The request body of <paramref name="streamId"/> as a read-only <see cref="Stream"/>.</summary>
    public Stream OpenRequestBody(ulong streamId) => new RequestBodyStream(this, streamId);

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);

    /// <summary>Frees the table. Services registered with it keep their own reference and keep working; their bodies are just unreachable from here.</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) == 0)
            Native.aster_http_bodies_free(_runtime.Handle, _handle);
    }

    private sealed class RequestBodyStream : Stream
    {
        private readonly HttpBodies _bodies;
        private readonly ulong _id;

        public RequestBodyStream(HttpBodies bodies, ulong id) { _bodies = bodies; _id = id; }

        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
        public override void Flush() { }
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();

        public override ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken cancellationToken = default)
            => _bodies.ReadAsync(_id, buffer, cancellationToken);

        public override Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken cancellationToken)
            => _bodies.ReadAsync(_id, buffer.AsMemory(offset, count), cancellationToken).AsTask();

        /// <summary>Blocks the calling thread; prefer <see cref="ReadAsync(Memory{byte}, CancellationToken)"/>.</summary>
        public override int Read(byte[] buffer, int offset, int count)
            => _bodies.ReadAsync(_id, buffer.AsMemory(offset, count)).AsTask().GetAwaiter().GetResult();
    }
}
