using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

public sealed class Tags
{
    private readonly Runtime _runtime;
    private readonly ulong _nodeHandle;

    internal Tags(Runtime runtime, ulong nodeHandle)
    {
        _runtime = runtime;
        _nodeHandle = nodeHandle;
    }

    public async Task SetAsync(string name, string hashHex, uint format = 0, CancellationToken ct = default)
    {
        byte[] nameBytes = Encoding.UTF8.GetBytes(name);
        byte[] hashBytes = Encoding.UTF8.GetBytes(hashHex);
        GCHandle namePin = GCHandle.Alloc(nameBytes, GCHandleType.Pinned);
        GCHandle hashPin = GCHandle.Alloc(hashBytes, GCHandleType.Pinned);
        try
        {
            int r = Native.iroh_tags_set(_runtime.Handle, _nodeHandle,
                namePin.AddrOfPinnedObject(), (UIntPtr)nameBytes.Length,
                hashPin.AddrOfPinnedObject(), (UIntPtr)hashBytes.Length,
                format, 0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_tags_set");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.TagSet) throw new IrohException($"set: unexpected event {ev.kind}");
        }
        finally { namePin.Free(); hashPin.Free(); }
    }

    public async Task<byte[]> GetAsync(string name, CancellationToken ct = default)
    {
        byte[] nameBytes = Encoding.UTF8.GetBytes(name);
        GCHandle namePin = GCHandle.Alloc(nameBytes, GCHandleType.Pinned);
        try
        {
            int r = Native.iroh_tags_get(_runtime.Handle, _nodeHandle,
                namePin.AddrOfPinnedObject(), (UIntPtr)nameBytes.Length, 0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_tags_get");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.TagGet) throw new IrohException($"get: unexpected event {ev.kind}");
            byte[] result = Array.Empty<byte>();
            if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
            {
                result = new byte[(int)ev.data_len];
                Marshal.Copy(ev.data_ptr, result, 0, (int)ev.data_len);
                if (ev.buffer != 0) _runtime.ReleaseBuffer(ev.buffer);
            }
            return result;
        }
        finally { namePin.Free(); }
    }

    public async Task DeleteAsync(string name, CancellationToken ct = default)
    {
        byte[] nameBytes = Encoding.UTF8.GetBytes(name);
        GCHandle namePin = GCHandle.Alloc(nameBytes, GCHandleType.Pinned);
        try
        {
            int r = Native.iroh_tags_delete(_runtime.Handle, _nodeHandle,
                namePin.AddrOfPinnedObject(), (UIntPtr)nameBytes.Length, 0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_tags_delete");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.TagDeleted) throw new IrohException($"delete: unexpected event {ev.kind}");
        }
        finally { namePin.Free(); }
    }
}
