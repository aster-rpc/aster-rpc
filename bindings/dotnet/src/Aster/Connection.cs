using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// Represents an active QUIC connection to a remote peer.
/// </summary>
public sealed partial class Connection : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ConnectionHandle _handle;
    private bool _disposed;

    internal Connection(Runtime runtime, ConnectionHandle handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>The native connection handle.</summary>
    public ConnectionHandle Handle => _handle;

    /// <summary>The runtime that owns this connection.</summary>
    public Runtime Runtime => _runtime;

    /// <summary>
    /// Opens a bidirectional stream on this connection.
    /// </summary>
    public async Task<(SendStream Send, RecvStream Recv)> OpenBiAsync(CancellationToken cancellationToken = default)
    {
        int r = Native.aster_open_bi(_runtime.Handle, _handle.Value, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_open_bi");

        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.StreamOpened)
            throw new IrohException($"open_bi: unexpected event {(EventKind)ev.kind}");

        // handle = send_stream, related = recv_stream
        return (new SendStream(_runtime, new SendStreamHandle(ev.handle)),
                new RecvStream(_runtime, new RecvStreamHandle(ev.related)));
    }

    /// <summary>
    /// Accepts a bidirectional stream on this connection.
    /// </summary>
    public async Task<(SendStream Send, RecvStream Recv)> AcceptBiAsync(CancellationToken cancellationToken = default)
    {
        int r = Native.aster_accept_bi(_runtime.Handle, _handle.Value, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_accept_bi");

        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.StreamAccepted)
            throw new IrohException($"accept_bi: unexpected event {(EventKind)ev.kind}");

        return (new SendStream(_runtime, new SendStreamHandle(ev.handle)),
                new RecvStream(_runtime, new RecvStreamHandle(ev.related)));
    }

    /// <summary>
    /// Opens a unidirectional stream on this connection (sender side).
    /// </summary>
    public async Task<SendStream> OpenUniAsync(CancellationToken cancellationToken = default)
    {
        int r = Native.aster_open_uni(_runtime.Handle, _handle.Value, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_open_uni");

        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.StreamOpened)
            throw new IrohException($"open_uni: unexpected event {(EventKind)ev.kind}");

        return new SendStream(_runtime, new SendStreamHandle(ev.handle));
    }

    /// <summary>
    /// Accepts a unidirectional stream on this connection (receiver side).
    /// </summary>
    public async Task<RecvStream> AcceptUniAsync(CancellationToken cancellationToken = default)
    {
        int r = Native.aster_accept_uni(_runtime.Handle, _handle.Value, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_accept_uni");

        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.StreamAccepted)
            throw new IrohException($"accept_uni: unexpected event {(EventKind)ev.kind}");

        return new RecvStream(_runtime, new RecvStreamHandle(ev.handle));
    }

    /// <summary>
    /// Gets the remote peer's node ID as a hex string.
    /// </summary>
    public string RemoteId()
    {
        IntPtr buf = Marshal.AllocHGlobal(65);
        try
        {
            int r = Native.aster_connection_remote_id(_runtime.Handle, _handle.Value, buf, (UIntPtr)65, out UIntPtr len);
            if (r != 0)
                throw IrohException.FromStatus(r, "aster_connection_remote_id");
            if (len == UIntPtr.Zero)
                return string.Empty;
            byte[] bytes = new byte[(int)len];
            Marshal.Copy(buf, bytes, 0, (int)len);
            return Encoding.UTF8.GetString(bytes); // native returns the hex id as UTF-8 text
        }
        finally
        {
            Marshal.FreeHGlobal(buf);
        }
    }

    /// <summary>
    /// Closes this connection with an optional reason string.
    /// </summary>
    public void Close(string reason = "")
    {
        byte[] reasonBytes = string.IsNullOrEmpty(reason) ? Array.Empty<byte>() : Encoding.UTF8.GetBytes(reason);
        int r;
        unsafe
        {
            fixed (byte* p = reasonBytes)
                r = Native.aster_connection_close(_runtime.Handle, _handle.Value, error_code: 0, Bytes.FromPointer(p, reasonBytes.Length));
        }
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_connection_close");
        _disposed = true;
    }

    /// <summary>
    /// Sends a datagram on this connection. Fire-and-forget; datagrams are unreliable.
    /// </summary>
    public void SendDatagram(byte[] data)
    {
        int r;
        unsafe
        {
            fixed (byte* p = data)
                r = Native.aster_connection_send_datagram(_runtime.Handle, _handle.Value, Bytes.FromPointer(p, data.Length));
        }
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_connection_send_datagram");
    }

    /// <summary>
    /// Reads the next datagram on this connection.
    /// </summary>
    public async Task<byte[]> ReadDatagramAsync(CancellationToken cancellationToken = default)
    {
        int r = Native.aster_connection_read_datagram(_runtime.Handle, _handle.Value, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_connection_read_datagram");

        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.DatagramReceived)
            throw new IrohException($"read_datagram: unexpected event {(EventKind)ev.kind}");

        byte[]? data = null;
        if (ev.buffer != 0 && ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
        {
            data = new byte[(int)ev.data_len];
            Marshal.Copy(ev.data_ptr, data, 0, (int)ev.data_len);
            _runtime.ReleaseBuffer(ev.buffer);
        }
        return data ?? Array.Empty<byte>();
    }

    public void Dispose()
    {
        if (!_disposed)
        {
            Close();
        }
    }

}
