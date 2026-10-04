using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.CompilerServices;
using System.Runtime.ExceptionServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Upcall;

namespace Aster.Expose;

/// <summary>Options of a streaming relay (<c>aster_http_relay_options_t</c>). Zero = the default.</summary>
public sealed record ExposeRelayOptions
{
    /// <summary>Largest chunk one upload write takes and one response read returns (default 1 MiB).</summary>
    public ulong MaxChunk { get; init; }

    /// <summary>Upload chunks queued before a write waits (default 8).</summary>
    public uint UploadCapacity { get; init; }
}

/// <summary>
/// A relay to a peer's exposed service failed with a stable relay code (<see cref="HttpBodyStatus"/> 10..16).
/// <see cref="IrohException.Status"/> is that code's <c>aster_status_t</c> (<see cref="Aster.Status.PermissionDenied"/> for a
/// denied relay, <see cref="Aster.Status.NotFound"/> for an unknown service, ...).
/// </summary>
public sealed class ExposeRelayException : IrohException
{
    public HttpBodyStatus RelayStatus { get; }

    internal ExposeRelayException(HttpBodyStatus relay, string what)
        : base((Status)Native.aster_http_body_status_to_status((int)relay), Describe(relay, what))
    {
        RelayStatus = relay;
    }

    private static string Describe(HttpBodyStatus relay, string what)
    {
        string? detail = LastErrorMessage();
        return string.IsNullOrEmpty(detail) ? $"{what}: {relay} ({(int)relay})" : $"{what}: {relay} ({(int)relay}): {detail}";
    }
}

/// <summary>
/// The response of <see cref="ExposeClient.RelayStreamAsync(Connection, string, string, string, IReadOnlyList{HttpHeader}?, IAsyncEnumerable{ReadOnlyMemory{byte}}?, ExposeRelayOptions?, CancellationToken)"/>:
/// the head (available as soon as the peer sends it, possibly before the upload is done) and a streamed
/// <see cref="Body"/>.
///
/// <para>Lifetime contract (full duplex): disposing before the body was read to its clean end abandons the
/// exchange and resets an unfinished upload (the handler sees a failed body, never a truncated one); a body
/// read to its clean end lets the upload finish (in the background; disposal returns at once and the pump frees the upload handle when it ends, so await <see cref="Upload"/> to observe it). A failure of the upload after the
/// head is reported through <see cref="Upload"/>, never thrown by the call that already returned.</para>
/// </summary>
public sealed class ExposeStreamResponse : IAsyncDisposable, IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _upload;
    private readonly ulong _response;
    private readonly int _maxChunk;
    private readonly CancellationTokenSource _pumpCts = new();
    private readonly Task<bool> _pump;
    private Exception? _sourceError;
    private int _disposed;
    private volatile bool _eof;
    private ushort _status;
    private IReadOnlyList<HttpHeader> _headers = Array.Empty<HttpHeader>();

    internal ExposeStreamResponse(Runtime runtime, ulong upload, ulong response, ulong maxChunk, IAsyncEnumerable<ReadOnlyMemory<byte>>? source)
    {
        _runtime = runtime;
        _upload = upload;
        _response = response;
        _maxChunk = (int)Math.Min(maxChunk == 0 ? 1 << 20 : maxChunk, int.MaxValue);
        _pump = Task.Run(() => RunPumpAsync(source));
        Body = new BodyStream(this);
    }

    /// <summary>HTTP status of the head.</summary>
    public ushort Status => _status;

    public IReadOnlyList<HttpHeader> Headers => _headers;

    /// <summary>The response body: a pull-based read-only stream (never blocks a pool thread).</summary>
    public Stream Body { get; }

    /// <summary>
    /// Completes with <c>true</c> when the whole request body was sent and finished, <c>false</c> when the
    /// peer stopped reading it first (an early answer such as 413) or the exchange was abandoned; faults with
    /// the body source's exception (or a transport error) when the upload failed.
    /// </summary>
    public Task<bool> Upload => _pump;

    internal Exception? SourceError => Volatile.Read(ref _sourceError);

    // ── native steps (never block: timeout 0) ────────────────────────────────

    private unsafe int WaitHeadOnce(out byte[] head)
    {
        head = Array.Empty<byte>();
        UIntPtr need = UIntPtr.Zero;
        int rc = Native.aster_http_response_wait(_runtime.Handle, _response, 0, null, UIntPtr.Zero, &need);
        if (rc != (int)HttpBodyStatus.BufferTooSmall) return rc;
        for (int attempt = 0; attempt < 4; attempt++)
        {
            byte[] buf = new byte[(int)(nuint)need];
            fixed (byte* p = buf)
                rc = Native.aster_http_response_wait(_runtime.Handle, _response, 0, p, (UIntPtr)buf.Length, &need);
            if (rc == (int)HttpBodyStatus.Ok) { head = buf.AsSpan(0, (int)(nuint)need).ToArray(); return rc; }
            if (rc != (int)HttpBodyStatus.BufferTooSmall) return rc;
        }
        return rc;
    }

    private unsafe int ReadOnce(Span<byte> buf, out int n, out bool eof)
    {
        UIntPtr len = UIntPtr.Zero;
        uint e = 0;
        int rc;
        fixed (byte* p = buf)
            rc = Native.aster_http_response_read(_runtime.Handle, _response, p, (UIntPtr)buf.Length, 0, &len, &e);
        n = (int)(nuint)len;
        eof = e != 0;
        return rc;
    }

    private unsafe int TryWriteOnce(ReadOnlySpan<byte> chunk, out bool queued)
    {
        uint q = 0;
        int rc;
        fixed (byte* p = chunk)
            rc = Native.aster_http_upload_try_write(_runtime.Handle, _upload, p, (UIntPtr)chunk.Length, &q);
        queued = q != 0;
        return rc;
    }

    /// <summary>Waits for a one-shot readiness event; returns the <c>aster_http_body_status_t</c> the next non-blocking call yields.</summary>
    private async Task<int> ReadyAsync(bool upload, CancellationToken ct)
    {
        int arm = upload
            ? Native.aster_http_upload_ready(_runtime.Handle, _upload, 0, out ulong op)
            : Native.aster_http_response_ready(_runtime.Handle, _response, 0, out op);
        if (arm != 0) throw IrohException.FromStatus(arm, upload ? "aster_http_upload_ready" : "aster_http_response_ready");
        try
        {
            Event ev = await _runtime.WaitForAsync(op, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.HttpRelayReady) throw EventError.From(_runtime, ev, "relay readiness");
            if (ev.buffer != 0) Native.aster_buffer_release(_runtime.Handle, ev.buffer);
            return ev.error_code;
        }
        catch (OperationCanceledException)
        {
            try { _runtime.Cancel(op); } catch (IrohException) { /* already fired */ }
            throw;
        }
    }

    // ── head ─────────────────────────────────────────────────────────────────

    internal async Task AwaitHeadAsync(CancellationToken ct)
    {
        while (true)
        {
            ct.ThrowIfCancellationRequested();
            int rc = WaitHeadOnce(out byte[] head);
            if (rc == (int)HttpBodyStatus.Ok)
            {
                var wire = Ucv1.DecodeExposeHttpResponse(head);
                _status = wire.Status;
                _headers = wire.Headers;
                return;
            }
            if (rc != (int)HttpBodyStatus.Timeout)
                throw new ExposeRelayException((HttpBodyStatus)rc, "aster_http_response_wait");
            await ReadyAsync(upload: false, ct).ConfigureAwait(false);
        }
    }

    // ── body ─────────────────────────────────────────────────────────────────

    private async ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken ct)
    {
        if (buffer.IsEmpty || _eof) return 0;
        while (true)
        {
            ct.ThrowIfCancellationRequested();
            ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);
            int rc = ReadOnce(buffer.Span, out int n, out bool eof);
            if (rc == (int)HttpBodyStatus.Ok)
            {
                if (eof) { _eof = true; return 0; }
                if (n > 0) return n;
            }
            else if (rc != (int)HttpBodyStatus.Timeout)
            {
                throw new HttpBodyException((HttpBodyStatus)rc, "aster_http_response_read", IrohException.LastErrorMessage());
            }
            await ReadyAsync(upload: false, ct).ConfigureAwait(false);
        }
    }

    // ── upload ───────────────────────────────────────────────────────────────

    /// <summary>
    /// The pump owns the upload handle and its cancellation source: both are released when it ends, however
    /// late. Cancellation reaches the body source through the enumerator / <c>Stream.ReadAsync</c> token; a
    /// source that ignores it keeps the pump (never <see cref="Dispose"/>) alive.
    /// </summary>
    private async Task<bool> RunPumpAsync(IAsyncEnumerable<ReadOnlyMemory<byte>>? source)
    {
        try
        {
            return await PumpAsync(source).ConfigureAwait(false);
        }
        finally
        {
            Native.aster_http_upload_free(_runtime.Handle, _upload); // aborts it if unfinished
            _pumpCts.Dispose();
        }
    }

    private async Task<bool> PumpAsync(IAsyncEnumerable<ReadOnlyMemory<byte>>? source)
    {
        var ct = _pumpCts.Token;
        try
        {
            if (source != null)
            {
                await foreach (ReadOnlyMemory<byte> chunk in source.WithCancellation(ct).ConfigureAwait(false))
                {
                    for (int off = 0; off < chunk.Length; off += _maxChunk)
                    {
                        var slice = chunk.Slice(off, Math.Min(_maxChunk, chunk.Length - off));
                        while (true)
                        {
                            ct.ThrowIfCancellationRequested();
                            int rc = TryWriteOnce(slice.Span, out bool queued);
                            if (rc == (int)HttpBodyStatus.Ok && queued) break;
                            if (rc != (int)HttpBodyStatus.Ok)
                                return StopOrThrow(rc);
                            int ready = await ReadyAsync(upload: true, ct).ConfigureAwait(false);
                            if (ready != (int)HttpBodyStatus.Ok) return StopOrThrow(ready);
                        }
                    }
                }
            }
            int fin = Native.aster_http_upload_finish(_runtime.Handle, _upload);
            if (fin != (int)HttpBodyStatus.Ok && fin != (int)HttpBodyStatus.Finished && fin != (int)HttpBodyStatus.Cancelled)
                throw new HttpBodyException((HttpBodyStatus)fin, "aster_http_upload_finish", IrohException.LastErrorMessage());
            return fin == (int)HttpBodyStatus.Ok;
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested)
        {
            return false; // abandoned by the caller / disposal
        }
        catch (Exception e)
        {
            // The body source (or the transport) failed: reset the request so the handler sees a failed body.
            Interlocked.CompareExchange(ref _sourceError, e, null);
            AbortUpload(e.Message);
            throw;
        }

        // The peer stopped reading (CANCELLED / FINISHED): not an error, stop producing.
        static bool StopOrThrow(int rc)
            => rc is (int)HttpBodyStatus.Cancelled or (int)HttpBodyStatus.Finished
                ? false
                : throw new HttpBodyException((HttpBodyStatus)rc, "aster_http_upload", IrohException.LastErrorMessage());
    }

    private unsafe void AbortUpload(string message)
    {
        byte[] m = Encoding.UTF8.GetBytes(message ?? "");
        fixed (byte* p = m)
            Native.aster_http_upload_abort(_runtime.Handle, _upload, m.Length == 0 ? null : p, (UIntPtr)m.Length);
    }

    // ── cleanup ──────────────────────────────────────────────────────────────

    /// <summary>Abandon before the head was returned: reset, stop the pump, free both handles.</summary>
    internal async Task AbandonAsync() => await DisposeCoreAsync(forceAbandon: true).ConfigureAwait(false);

    public ValueTask DisposeAsync() => new(DisposeCoreAsync(forceAbandon: false));

    public void Dispose() => DisposeCoreAsync(forceAbandon: false).GetAwaiter().GetResult();

    private async Task DisposeCoreAsync(bool forceAbandon)
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        if (_eof && !forceAbandon)
        {
            // A body read to its clean end leaves the upload to finish -- in the background: the pump owns
            // the upload handle and frees it when it ends (its outcome stays on Upload), so disposal never
            // waits on a stalled body source.
            Native.aster_http_response_free(_runtime.Handle, _response);
            return;
        }
        // Closing the response resets an unfinished upload (the handler sees a failed body).
        Native.aster_http_response_close(_runtime.Handle, _response);
        try { _pumpCts.Cancel(); } catch (ObjectDisposedException) { /* the pump already ended */ }
        // A bounded courtesy wait; a source that ignores cancellation keeps the pump (not this call) alive,
        // and the pump frees the upload handle when it finally ends.
        try { await _pump.WaitAsync(TimeSpan.FromSeconds(5)).ConfigureAwait(false); } catch (Exception) { }
        Native.aster_http_response_free(_runtime.Handle, _response);
    }

    private sealed class BodyStream(ExposeStreamResponse owner) : Stream
    {
        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
        public override void Flush() { }
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        public override ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken cancellationToken = default) => owner.ReadAsync(buffer, cancellationToken);
        public override Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken cancellationToken)
            => owner.ReadAsync(buffer.AsMemory(offset, count), cancellationToken).AsTask();
        /// <summary>Blocks the calling thread; prefer the async overloads.</summary>
        public override int Read(byte[] buffer, int offset, int count) => owner.ReadAsync(buffer.AsMemory(offset, count), default).AsTask().GetAwaiter().GetResult();
    }
}
