using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

public sealed class Doc : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private bool _disposed;

    internal Doc(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    public ulong Handle => _handle;

    internal Runtime Runtime => _runtime;

    public async Task SetBytesAsync(string authorId, string key, byte[] value, CancellationToken ct = default)
    {
        byte[] authorBytes = Encoding.UTF8.GetBytes(authorId);
        byte[] keyBytes = Encoding.UTF8.GetBytes(key);
        GCHandle authorPin = GCHandle.Alloc(authorBytes, GCHandleType.Pinned);
        GCHandle keyPin = GCHandle.Alloc(keyBytes, GCHandleType.Pinned);
        GCHandle valuePin = GCHandle.Alloc(value, GCHandleType.Pinned);
        try
        {
            int r = Native.iroh_doc_set_bytes(_runtime.Handle, _handle,
                authorPin.AddrOfPinnedObject(), (UIntPtr)authorBytes.Length,
                keyPin.AddrOfPinnedObject(), (UIntPtr)keyBytes.Length,
                valuePin.AddrOfPinnedObject(), (UIntPtr)value.Length,
                0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_doc_set_bytes");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.DocSet) throw new IrohException($"set_bytes: unexpected event {ev.kind}");
        }
        finally { authorPin.Free(); keyPin.Free(); valuePin.Free(); }
    }

    public async Task<byte[]> GetExactAsync(string authorId, string key, CancellationToken ct = default)
    {
        byte[] authorBytes = Encoding.UTF8.GetBytes(authorId);
        byte[] keyBytes = Encoding.UTF8.GetBytes(key);
        GCHandle authorPin = GCHandle.Alloc(authorBytes, GCHandleType.Pinned);
        GCHandle keyPin = GCHandle.Alloc(keyBytes, GCHandleType.Pinned);
        try
        {
            int r = Native.iroh_doc_get_exact(_runtime.Handle, _handle,
                authorPin.AddrOfPinnedObject(), (UIntPtr)authorBytes.Length,
                keyPin.AddrOfPinnedObject(), (UIntPtr)keyBytes.Length,
                0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_doc_get_exact");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.DocGet) throw new IrohException($"get_exact: unexpected event {ev.kind}");
            byte[] result = Array.Empty<byte>();
            if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
            {
                result = new byte[(int)ev.data_len];
                Marshal.Copy(ev.data_ptr, result, 0, (int)ev.data_len);
                if (ev.buffer != 0) _runtime.ReleaseBuffer(ev.buffer);
            }
            return result;
        }
        finally { authorPin.Free(); keyPin.Free(); }
    }

    public async Task<string> ShareAsync(uint mode = 0, CancellationToken ct = default)
    {
        int r = Native.iroh_doc_share(_runtime.Handle, _handle, mode, 0, out ulong opId);
        if (r != 0) throw IrohException.FromStatus(r, "iroh_doc_share");
        Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.DocShared) throw new IrohException($"share: unexpected event {ev.kind}");
        string ticket = string.Empty;
        if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
        {
            byte[] data = new byte[(int)ev.data_len];
            Marshal.Copy(ev.data_ptr, data, 0, (int)ev.data_len);
            ticket = Encoding.UTF8.GetString(data);
            if (ev.buffer != 0) _runtime.ReleaseBuffer(ev.buffer);
        }
        return ticket;
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        Native.iroh_doc_free(_runtime.Handle, _handle);
    }
}

public sealed class Docs
{
    private readonly Runtime _runtime;
    private readonly ulong _nodeHandle;

    internal Docs(Runtime runtime, ulong nodeHandle)
    {
        _runtime = runtime;
        _nodeHandle = nodeHandle;
    }

    public async Task<Doc> CreateAsync(CancellationToken ct = default)
    {
        int r = Native.iroh_docs_create(_runtime.Handle, _nodeHandle, 0, out ulong opId);
        if (r != 0) throw IrohException.FromStatus(r, "iroh_docs_create");
        Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.DocCreated) throw new IrohException($"create: unexpected event {ev.kind}");
        return new Doc(_runtime, ev.handle);
    }

    public async Task<string> CreateAuthorAsync(CancellationToken ct = default)
    {
        int r = Native.iroh_docs_create_author(_runtime.Handle, _nodeHandle, 0, out ulong opId);
        if (r != 0) throw IrohException.FromStatus(r, "iroh_docs_create_author");
        Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.AuthorCreated) throw new IrohException($"create_author: unexpected event {ev.kind}");
        string authorId = string.Empty;
        if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
        {
            byte[] data = new byte[(int)ev.data_len];
            Marshal.Copy(ev.data_ptr, data, 0, (int)ev.data_len);
            authorId = Encoding.UTF8.GetString(data);
            if (ev.buffer != 0) _runtime.ReleaseBuffer(ev.buffer);
        }
        return authorId;
    }

    public async Task<Doc> JoinAsync(string ticket, CancellationToken ct = default)
    {
        byte[] ticketBytes = Encoding.UTF8.GetBytes(ticket);
        GCHandle pin = GCHandle.Alloc(ticketBytes, GCHandleType.Pinned);
        try
        {
            int r = Native.iroh_docs_join(_runtime.Handle, _nodeHandle, pin.AddrOfPinnedObject(), (UIntPtr)ticketBytes.Length, 0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_docs_join");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.DocJoined) throw new IrohException($"join: unexpected event {ev.kind}");
            return new Doc(_runtime, ev.handle);
        }
        finally { pin.Free(); }
    }
}
