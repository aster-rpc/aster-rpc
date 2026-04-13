using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

public sealed class Blobs
{
    private readonly Runtime _runtime;
    private readonly ulong _nodeHandle;

    internal Blobs(Runtime runtime, ulong nodeHandle)
    {
        _runtime = runtime;
        _nodeHandle = nodeHandle;
    }

    public async Task<(string Hash, ulong Size)> AddBytesAsync(byte[] data, CancellationToken ct = default)
    {
        GCHandle pin = GCHandle.Alloc(data, GCHandleType.Pinned);
        try
        {
            int r = Native.iroh_blobs_add_bytes(_runtime.Handle, _nodeHandle, pin.AddrOfPinnedObject(), (UIntPtr)data.Length, 0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_blobs_add_bytes");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.BlobAdded) throw new IrohException($"add_bytes: unexpected event {ev.kind}");
            // hash is in data_ptr, size is in related
            string hash = string.Empty;
            if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
            {
                byte[] hashBytes = new byte[(int)ev.data_len];
                Marshal.Copy(ev.data_ptr, hashBytes, 0, (int)ev.data_len);
                hash = Encoding.UTF8.GetString(hashBytes);
                if (ev.buffer != 0) _runtime.ReleaseBuffer(ev.buffer);
            }
            return (hash, ev.related);
        }
        finally { pin.Free(); }
    }

    public async Task<byte[]> ReadAsync(string hashHex, CancellationToken ct = default)
    {
        byte[] hashBytes = Encoding.UTF8.GetBytes(hashHex);
        GCHandle pin = GCHandle.Alloc(hashBytes, GCHandleType.Pinned);
        try
        {
            int r = Native.iroh_blobs_read(_runtime.Handle, _nodeHandle, pin.AddrOfPinnedObject(), (UIntPtr)hashBytes.Length, 0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_blobs_read");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.BlobRead) throw new IrohException($"read: unexpected event {ev.kind}");
            byte[] result = Array.Empty<byte>();
            if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
            {
                result = new byte[(int)ev.data_len];
                Marshal.Copy(ev.data_ptr, result, 0, (int)ev.data_len);
                if (ev.buffer != 0) _runtime.ReleaseBuffer(ev.buffer);
            }
            return result;
        }
        finally { pin.Free(); }
    }

    public async Task<string> DownloadAsync(string ticket, CancellationToken ct = default)
    {
        byte[] ticketBytes = Encoding.UTF8.GetBytes(ticket);
        GCHandle pin = GCHandle.Alloc(ticketBytes, GCHandleType.Pinned);
        try
        {
            int r = Native.iroh_blobs_download(_runtime.Handle, _nodeHandle, pin.AddrOfPinnedObject(), (UIntPtr)ticketBytes.Length, 0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_blobs_download");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.BlobDownloaded) throw new IrohException($"download: unexpected event {ev.kind}");
            string hash = string.Empty;
            if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
            {
                byte[] hashBytes = new byte[(int)ev.data_len];
                Marshal.Copy(ev.data_ptr, hashBytes, 0, (int)ev.data_len);
                hash = Encoding.UTF8.GetString(hashBytes);
                if (ev.buffer != 0) _runtime.ReleaseBuffer(ev.buffer);
            }
            return hash;
        }
        finally { pin.Free(); }
    }
}
