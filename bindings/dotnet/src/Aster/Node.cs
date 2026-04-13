using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// High-level Iroh node with all protocols enabled.
/// </summary>
public sealed class Node : IDisposable
{
    private readonly Runtime _runtime;
    private readonly NodeHandle _handle;
    private bool _disposed;

    internal Node(Runtime runtime, NodeHandle handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>The runtime that owns this node.</summary>
    public Runtime Runtime => _runtime;

    /// <summary>The native node handle.</summary>
    public NodeHandle Handle => _handle;

    /// <summary>Gets this node's ID as a hex string.</summary>
    public string NodeId()
    {
        IntPtr buf = Marshal.AllocHGlobal(65);
        try
        {
            int r = Native.iroh_node_id(_runtime.Handle, _handle.Value, buf, (UIntPtr)65, out UIntPtr len);
            if (r != 0)
                throw IrohException.FromStatus(r, "iroh_node_id");
            if (len == UIntPtr.Zero)
                return string.Empty;
            byte[] bytes = new byte[(int)len];
            Marshal.Copy(buf, bytes, 0, (int)len);
            return Encoding_hex(bytes);
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    /// <summary>Gets this node's structured address info.</summary>
    public NodeAddrInfo NodeAddr()
    {
        IntPtr buf = Marshal.AllocHGlobal(4096);
        try
        {
            int r = Native.iroh_node_addr_info(_runtime.Handle, _handle.Value, buf, (UIntPtr)4096, out NodeAddr addr);
            if (r != 0)
                throw IrohException.FromStatus(r, "iroh_node_addr_info");

            string endpointId = ReadBytes(addr.endpoint_id);
            string? relayUrl = ReadBytesOpt(addr.relay_url);

            return new NodeAddrInfo(endpointId, relayUrl);
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    /// <summary>Get the blob store for this node.</summary>
    public Blobs Blobs() => new Blobs(_runtime, _handle.Value);

    /// <summary>Get the docs operations for this node.</summary>
    public Docs Docs() => new Docs(_runtime, _handle.Value);

    /// <summary>Get the gossip operations for this node.</summary>
    public Gossip Gossip() => new Gossip(_runtime, _handle.Value);

    /// <summary>Get the tags operations for this node.</summary>
    public Tags Tags() => new Tags(_runtime, _handle.Value);

    /// <summary>Closes this node and frees its handle.</summary>
    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        Native.iroh_node_free(_runtime.Handle, _handle.Value);
    }

    /// <summary>Creates an in-memory node with the given ALPNs.</summary>
    public static async Task<Node> MemoryWithAlpnsAsync(string[] alpns, CancellationToken cancellationToken = default)
    {
        var runtime = new Runtime();
        try
        {
            ulong opId;
            unsafe
            {
                byte[][] alpnBytes = new byte[alpns.Length][];
                for (int i = 0; i < alpns.Length; i++)
                    alpnBytes[i] = Encoding.UTF8.GetBytes(alpns[i]);

                // Allocate native memory for pointers and lengths
                IntPtr ptrsPtr = Marshal.AllocHGlobal(IntPtr.Size * alpns.Length);
                IntPtr lensPtr = Marshal.AllocHGlobal(sizeof(UIntPtr) * alpns.Length);
                var pins = new GCHandle[alpns.Length];

                try
                {
                    for (int i = 0; i < alpns.Length; i++)
                    {
                        pins[i] = GCHandle.Alloc(alpnBytes[i], GCHandleType.Pinned);
                        Marshal.WriteIntPtr(ptrsPtr, i * IntPtr.Size, pins[i].AddrOfPinnedObject());
                        if (IntPtr.Size == 8)
                            Marshal.WriteInt64(lensPtr, i * sizeof(UIntPtr), alpnBytes[i].Length);
                        else
                            Marshal.WriteInt32(lensPtr, i * sizeof(UIntPtr), alpnBytes[i].Length);
                    }

                    int r = Native.iroh_node_memory_with_alpns(
                        runtime.Handle,
                        (byte**)ptrsPtr, (UIntPtr*)lensPtr, (UIntPtr)alpns.Length,
                        0, out opId);
                    if (r != 0)
                    {
                        runtime.Dispose();
                        throw IrohException.FromStatus(r, "iroh_node_memory_with_alpns");
                    }
                }
                finally
                {
                    foreach (var p in pins)
                        if (p.IsAllocated) p.Free();
                    Marshal.FreeHGlobal(ptrsPtr);
                    Marshal.FreeHGlobal(lensPtr);
                }
            }

            Event ev = await runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
            if (ev.kind != 1) // NODE_CREATED
            {
                runtime.Dispose();
                throw new IrohException($"node create: unexpected event kind {ev.kind}");
            }

            return new Node(runtime, new NodeHandle(ev.handle));
        }
        catch
        {
            runtime.Dispose();
            throw;
        }
    }

    private static string ReadBytes(Bytes b)
    {
        if (b.ptr == IntPtr.Zero || b.len == UIntPtr.Zero)
            return string.Empty;
        byte[] data = new byte[(int)b.len];
        Marshal.Copy(b.ptr, data, 0, (int)b.len);
        return Encoding.UTF8.GetString(data);
    }

    private static string? ReadBytesOpt(Bytes b)
    {
        if (b.ptr == IntPtr.Zero || b.len == UIntPtr.Zero)
            return null;
        byte[] data = new byte[(int)b.len];
        Marshal.Copy(b.ptr, data, 0, (int)b.len);
        return Encoding.UTF8.GetString(data);
    }

    private static string Encoding_hex(byte[] data)
    {
        var sb = new StringBuilder(data.Length * 2);
        foreach (var b in data) sb.AppendFormat("{0:x2}", b);
        return sb.ToString();
    }
}

/// <summary>Structured node address info.</summary>
public record NodeAddrInfo(string EndpointId, string? RelayUrl);
