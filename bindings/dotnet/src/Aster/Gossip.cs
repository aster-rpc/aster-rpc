using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

public sealed class GossipTopic : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private bool _disposed;

    internal GossipTopic(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    public async Task BroadcastAsync(byte[] data, CancellationToken ct = default)
    {
        GCHandle pin = GCHandle.Alloc(data, GCHandleType.Pinned);
        try
        {
            var dataBytes = new Bytes { ptr = pin.AddrOfPinnedObject(), len = (UIntPtr)data.Length };
            int r = Native.iroh_gossip_broadcast(_runtime.Handle, _handle, dataBytes, 0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_gossip_broadcast");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.GossipBroadcastDone)
                throw new IrohException($"broadcast: unexpected event {ev.kind}");
        }
        finally { pin.Free(); }
    }

    public async Task<byte[]> RecvAsync(CancellationToken ct = default)
    {
        int r = Native.iroh_gossip_recv(_runtime.Handle, _handle, 0, out ulong opId);
        if (r != 0) throw IrohException.FromStatus(r, "iroh_gossip_recv");
        Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.GossipReceived)
            throw new IrohException($"recv: unexpected event {ev.kind}");
        byte[] result = Array.Empty<byte>();
        if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
        {
            result = new byte[(int)ev.data_len];
            Marshal.Copy(ev.data_ptr, result, 0, (int)ev.data_len);
            if (ev.buffer != 0) _runtime.ReleaseBuffer(ev.buffer);
        }
        return result;
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        Native.iroh_gossip_topic_free(_runtime.Handle, _handle);
    }
}

public sealed class Gossip
{
    private readonly Runtime _runtime;
    private readonly ulong _nodeHandle;

    internal Gossip(Runtime runtime, ulong nodeHandle)
    {
        _runtime = runtime;
        _nodeHandle = nodeHandle;
    }

    public async Task<GossipTopic> SubscribeAsync(string topic, string[]? peerIds = null, CancellationToken ct = default)
    {
        byte[] topicBytes = Encoding.UTF8.GetBytes(topic);
        GCHandle topicPin = GCHandle.Alloc(topicBytes, GCHandleType.Pinned);
        var topicNative = new Bytes { ptr = topicPin.AddrOfPinnedObject(), len = (UIntPtr)topicBytes.Length };

        try
        {
            var peers = BytesList.Empty;
            int r = Native.iroh_gossip_subscribe(_runtime.Handle, _nodeHandle, topicNative, peers, 0, out ulong opId);
            if (r != 0) throw IrohException.FromStatus(r, "iroh_gossip_subscribe");
            Event ev = await _runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.GossipSubscribed)
                throw new IrohException($"subscribe: unexpected event {ev.kind}");
            return new GossipTopic(_runtime, ev.handle);
        }
        finally { topicPin.Free(); }
    }
}
