using System;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// A unidirectional send stream. Send streams are used to send frames to a remote peer.
/// </summary>
public sealed class SendStream : IDisposable
{
    private readonly Runtime _runtime;
    private readonly SendStreamHandle _handle;
    private bool _disposed;

    internal SendStream(Runtime runtime, SendStreamHandle handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>The native send stream handle.</summary>
    public SendStreamHandle Handle => _handle;

    /// <summary>
    /// Sends a framed payload on this stream.
    /// Completes when the remote acknowledges the send.
    /// </summary>
    public async Task SendAsync(byte[] data, CancellationToken cancellationToken = default)
    {
        Bytes dataNative = Bytes.FromArray(data);
        int r = Native.iroh_stream_write(_runtime.Handle, _handle.Value, dataNative, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_stream_write");

        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.SendCompleted)
            throw new IrohException($"send: unexpected event {(EventKind)ev.kind}");
    }

    /// <summary>
    /// Finishes this stream's send side — signals no more frames will be sent.
    /// </summary>
    public async Task FinishAsync(CancellationToken cancellationToken = default)
    {
        int r = Native.iroh_stream_finish(_runtime.Handle, _handle.Value, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_stream_finish");

        await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
    }

    /// <summary>
    /// Resets this stream with a QUIC error code.
    /// </summary>
    public void Reset(int errorCode = 0)
    {
        int r = Native.iroh_stream_stop(_runtime.Handle, _handle.Value, (uint)errorCode);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_stream_stop");
    }

    public void Dispose()
    {
        if (!_disposed)
        {
            _disposed = true;
            try
            {
                Native.iroh_send_stream_free(_runtime.Handle, _handle.Value);
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine($"send_stream_free failed: {ex.Message}");
            }
        }
    }
}

/// <summary>
/// A unidirectional receive stream. Receive streams are used to receive frames from a remote peer.
/// </summary>
public sealed class RecvStream : IDisposable
{
    private readonly Runtime _runtime;
    private readonly RecvStreamHandle _handle;
    private bool _disposed;

    internal RecvStream(Runtime runtime, RecvStreamHandle handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>The native recv stream handle.</summary>
    public RecvStreamHandle Handle => _handle;

    /// <summary>
    /// Reads the next frame from this stream.
    /// </summary>
    public async Task<byte[]> ReadAsync(long maxLen = 65536, CancellationToken cancellationToken = default)
    {
        int r = Native.iroh_stream_read(_runtime.Handle, _handle.Value, (UIntPtr)maxLen, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_stream_read");

        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.FrameReceived)
            throw new IrohException($"read: unexpected event {(EventKind)ev.kind}");

        byte[]? data = null;
        if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
        {
            data = new byte[(int)ev.data_len];
            Marshal.Copy(ev.data_ptr, data, 0, (int)ev.data_len);
            if (ev.buffer != 0)
                _runtime.ReleaseBuffer(ev.buffer);
        }
        return data ?? Array.Empty<byte>();
    }

    /// <summary>
    /// Reads all remaining data from this stream until it finishes.
    /// </summary>
    public async Task<byte[]> ReadToEndAsync(long maxSize = 1024 * 1024, CancellationToken cancellationToken = default)
    {
        int r = Native.iroh_stream_read_to_end(_runtime.Handle, _handle.Value, (UIntPtr)maxSize, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_stream_read_to_end");

        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.FrameReceived)
            throw new IrohException($"read_to_end: unexpected event {(EventKind)ev.kind}");

        byte[]? data = null;
        if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
        {
            data = new byte[(int)ev.data_len];
            Marshal.Copy(ev.data_ptr, data, 0, (int)ev.data_len);
            if (ev.buffer != 0)
                _runtime.ReleaseBuffer(ev.buffer);
        }
        return data ?? Array.Empty<byte>();
    }

    /// <summary>
    /// Stops this recv stream with a QUIC error code.
    /// </summary>
    public void Stop(int errorCode = 0)
    {
        int r = Native.iroh_stream_stop(_runtime.Handle, _handle.Value, (uint)errorCode);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_stream_stop");
    }

    public void Dispose()
    {
        if (!_disposed)
        {
            _disposed = true;
            try
            {
                Native.iroh_recv_stream_free(_runtime.Handle, _handle.Value);
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine($"recv_stream_free failed: {ex.Message}");
            }
        }
    }
}
