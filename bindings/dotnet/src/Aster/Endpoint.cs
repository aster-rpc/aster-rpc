using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

internal sealed class NativePointer : IDisposable
{
    private readonly IntPtr _ptr;
    public NativePointer(IntPtr ptr) { _ptr = ptr; }
    public void Dispose() { Marshal.FreeHGlobal(_ptr); }
}

internal sealed class GCHandleRef : IDisposable
{
    private readonly GCHandle _handle;
    public GCHandleRef(GCHandle handle) { _handle = handle; }
    public void Dispose() { _handle.Free(); }
}

/// <summary>
/// Represents an iroh endpoint, which is a local QUIC endpoint capable of
/// connecting to remote peers or accepting connections from them.
/// </summary>
public sealed class Endpoint : IDisposable
{
    private readonly Runtime _runtime;
    private readonly EndpointHandle _handle;
    private bool _disposed;

    internal Endpoint(Runtime runtime, EndpointHandle handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>The native endpoint handle.</summary>
    public EndpointHandle Handle => _handle;

    /// <summary>The runtime that owns this endpoint.</summary>
    public Runtime Runtime => _runtime;

    /// <summary>
    /// Creates a new endpoint with the given builder.
    /// All config data is allocated in native memory and remains valid across the async call.
    /// </summary>
    public static async Task<Endpoint> CreateAsync(Runtime runtime, EndpointConfigBuilder builder, CancellationToken cancellationToken = default)
    {
        var (configPtr, cleanup) = BuildNativeConfig(builder);
        try
        {
            int r = Native.iroh_endpoint_create(runtime.Handle, configPtr, user_data: 0, out ulong opId);
            if (r != 0)
                throw IrohException.FromStatus(r, "iroh_endpoint_create");

            Event ev = await runtime.WaitForAsync(opId, cancellationToken);
            if (ev.kind != (uint)EventKind.EndpointCreated)
                throw new IrohException($"endpoint create: unexpected event {(EventKind)ev.kind}");

            return new Endpoint(runtime, new EndpointHandle(ev.handle));
        }
        finally
        {
            // Cleanup is disposed by NativePointer handles which free the native memory.
            foreach (var c in cleanup) c.Dispose();
        }
    }

    /// <summary>Creates a new endpoint with default config.</summary>
    public static Task<Endpoint> CreateAsync(Runtime runtime, CancellationToken cancellationToken = default)
        => CreateAsync(runtime, new EndpointConfigBuilder(), cancellationToken);

    /// <summary>
    /// Allocates EndpointConfig in native memory with all nested data pinned.
    /// Returns (configPtr, cleanup handles). Caller must free configPtr and invoke cleanup.
    /// </summary>
    private static (IntPtr configPtr, List<IDisposable> cleanup) BuildNativeConfig(EndpointConfigBuilder builder)
    {
        var cleanup = new List<IDisposable>();

        // Build alpns array in native memory.
        int alpnCount = builder.AlpnCount;
        IntPtr alpnsArrayPtr = alpnCount > 0
            ? Marshal.AllocHGlobal(Marshal.SizeOf<Bytes>() * alpnCount)
            : IntPtr.Zero;
        if (alpnCount > 0) cleanup.Add(new NativePointer(alpnsArrayPtr));

        // Pin each ALPN and write into the native array.
        var pinnedAlpns = new List<GCHandle>();
        var alpnBytes = new List<byte[]>();
        IntPtr current = alpnsArrayPtr;
        int bytesStructSize = Marshal.SizeOf<Bytes>();
        foreach (var (alpnPtr, alpnLen) in builder.EnumerateAlpns())
        {
            var handle = GCHandle.Alloc(alpnPtr, GCHandleType.Pinned);
            pinnedAlpns.Add(handle);
            alpnBytes.Add(alpnPtr);
            Marshal.StructureToPtr(new Bytes { ptr = handle.AddrOfPinnedObject(), len = (UIntPtr)alpnLen }, current, fDeleteOld: false);
            current = IntPtr.Add(current, bytesStructSize);
        }
        foreach (var h in pinnedAlpns) cleanup.Add(new GCHandleRef(h));

        // Build the config struct in native memory.
        IntPtr configPtr = Marshal.AllocHGlobal(Marshal.SizeOf<EndpointConfig>());
        cleanup.Add(new NativePointer(configPtr));

        var config = new EndpointConfig
        {
            struct_size = (uint)Marshal.SizeOf<EndpointConfig>(),
            relay_mode = (uint)builder.RelayModeValue,
            secret_key = default,
            alpns = new BytesList { items = alpnsArrayPtr, len = (UIntPtr)alpnCount },
            relay_urls = BytesList.Empty,
            enable_discovery = builder.EnableDiscoveryValue ? 1u : 0u,
            enable_hooks = 0,
            hook_timeout_ms = 0,
            bind_addr = default,
            clear_ip_transports = 0,
            clear_relay_transports = 0,
            portmapper_config = 0,
            proxy_url = default,
            proxy_from_env = 0,
            data_dir_utf8 = default,
        };
        Marshal.StructureToPtr(config, configPtr, fDeleteOld: false);

        return (configPtr, cleanup);
    }

    /// <summary>Gets this endpoint's node ID as a hex string.</summary>
    public string NodeId()
    {
        IntPtr buf = Marshal.AllocHGlobal(65);
        try
        {
            int r = Native.iroh_endpoint_id(_runtime.Handle, _handle.Value, buf, (UIntPtr)65, out UIntPtr len);
            if (r != 0)
                throw IrohException.FromStatus(r, "iroh_endpoint_id");
            if (len == UIntPtr.Zero)
                return string.Empty;
            byte[] bytes = new byte[(int)len];
            Marshal.Copy(buf, bytes, 0, (int)len);
            return Encoding_hex(bytes);
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    /// <summary>Closes this endpoint asynchronously.</summary>
    public async Task CloseAsync(CancellationToken cancellationToken = default)
    {
        int r = Native.iroh_endpoint_close(_runtime.Handle, _handle.Value, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_endpoint_close");

        await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        _disposed = true;
    }

    /// <summary>Closes this endpoint synchronously.</summary>
    /// <remarks>
    /// Uses <see cref="Task.Run"/> to avoid sync-over-async deadlock when called
    /// from a synchronization context. The async close operation is dispatched to
    /// the thread pool, and we block until it completes.
    /// </remarks>
    public void Close()
    {
        try { Task.Run(() => CloseAsync()).GetAwaiter().GetResult(); }
        catch (AggregateException ex) { throw ex.InnerException!; }
    }

    /// <summary>Accepts an incoming connection on this endpoint.</summary>
    public async Task<Connection> AcceptAsync(CancellationToken cancellationToken = default)
    {
        int r = Native.iroh_accept(_runtime.Handle, _handle.Value, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_accept");

        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.ConnectionAccepted)
            throw new IrohException($"accept: unexpected event {(EventKind)ev.kind}");

        return new Connection(_runtime, new ConnectionHandle(ev.handle));
    }

    /// <summary>Connects to a remote node by node ID (hex) and ALPN.</summary>
    public async Task<Connection> ConnectAsync(string nodeIdHex, string alpn, CancellationToken cancellationToken = default)
    {
        byte[] nodeIdBytes = Encoding_hex_decode(nodeIdHex);
        byte[] alpnBytes = Encoding.UTF8.GetBytes(alpn);

        GCHandle nodeIdHandle = GCHandle.Alloc(nodeIdBytes, GCHandleType.Pinned);
        GCHandle alpnHandle = GCHandle.Alloc(alpnBytes, GCHandleType.Pinned);
        try
        {
            // Build ConnectConfig in native memory.
            IntPtr configPtr = Marshal.AllocHGlobal(Marshal.SizeOf<ConnectConfig>());
            try
            {
                var config = new ConnectConfig
                {
                    struct_size = (uint)Marshal.SizeOf<ConnectConfig>(),
                    node_id = new Bytes { ptr = nodeIdHandle.AddrOfPinnedObject(), len = (UIntPtr)nodeIdBytes.Length },
                    alpn = new Bytes { ptr = alpnHandle.AddrOfPinnedObject(), len = (UIntPtr)alpnBytes.Length },
                };
                Marshal.StructureToPtr(config, configPtr, fDeleteOld: false);

                int r = Native.iroh_connect(_runtime.Handle, _handle.Value, configPtr, user_data: 0, out ulong opId);
                if (r != 0)
                    throw IrohException.FromStatus(r, "iroh_connect");

                Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
                if (ev.kind != (uint)EventKind.Connected)
                    throw new IrohException($"connect: unexpected event {(EventKind)ev.kind}");

                return new Connection(_runtime, new ConnectionHandle(ev.handle));
            }
            finally { Marshal.FreeHGlobal(configPtr); }
        }
        finally { nodeIdHandle.Free(); alpnHandle.Free(); }
    }

    /// <summary>Connects to a remote node using its full address info.</summary>
    public async Task<Connection> ConnectWithAddrAsync(NodeAddrInfo addr, string alpn, CancellationToken cancellationToken = default)
    {
        byte[] nodeIdBytes = Encoding.UTF8.GetBytes(addr.EndpointId);
        byte[] alpnBytes = Encoding.UTF8.GetBytes(alpn);

        // Build NodeAddr struct with relay URL
        byte[]? relayBytes = addr.RelayUrl != null ? Encoding.UTF8.GetBytes(addr.RelayUrl) : null;

        GCHandle nodeIdHandle = GCHandle.Alloc(nodeIdBytes, GCHandleType.Pinned);
        GCHandle alpnHandle = GCHandle.Alloc(alpnBytes, GCHandleType.Pinned);
        GCHandle relayHandle = relayBytes != null ? GCHandle.Alloc(relayBytes, GCHandleType.Pinned) : default;
        try
        {
            // Build the NodeAddr to pass via ConnectConfig.addr
            var nodeAddr = new NodeAddr
            {
                endpoint_id = new Bytes { ptr = nodeIdHandle.AddrOfPinnedObject(), len = (UIntPtr)nodeIdBytes.Length },
                relay_url = relayBytes != null
                    ? new Bytes { ptr = relayHandle.AddrOfPinnedObject(), len = (UIntPtr)relayBytes.Length }
                    : default,
                direct_addresses = BytesList.Empty,
            };

            IntPtr nodeAddrPtr = Marshal.AllocHGlobal(Marshal.SizeOf<NodeAddr>());
            Marshal.StructureToPtr(nodeAddr, nodeAddrPtr, fDeleteOld: false);

            IntPtr configPtr = Marshal.AllocHGlobal(Marshal.SizeOf<ConnectConfig>());
            try
            {
                var config = new ConnectConfig
                {
                    struct_size = (uint)Marshal.SizeOf<ConnectConfig>(),
                    node_id = new Bytes { ptr = nodeIdHandle.AddrOfPinnedObject(), len = (UIntPtr)nodeIdBytes.Length },
                    alpn = new Bytes { ptr = alpnHandle.AddrOfPinnedObject(), len = (UIntPtr)alpnBytes.Length },
                    addr = nodeAddrPtr,
                };
                Marshal.StructureToPtr(config, configPtr, fDeleteOld: false);

                int r = Native.iroh_connect(_runtime.Handle, _handle.Value, configPtr, user_data: 0, out ulong opId);
                if (r != 0)
                    throw IrohException.FromStatus(r, "iroh_connect");

                Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
                if (ev.kind != (uint)EventKind.Connected)
                    throw new IrohException($"connect: unexpected event {(EventKind)ev.kind}");

                return new Connection(_runtime, new ConnectionHandle(ev.handle));
            }
            finally
            {
                Marshal.FreeHGlobal(configPtr);
                Marshal.FreeHGlobal(nodeAddrPtr);
            }
        }
        finally
        {
            nodeIdHandle.Free();
            alpnHandle.Free();
            if (relayHandle.IsAllocated) relayHandle.Free();
        }
    }

    public void Dispose() { if (!_disposed) Close(); }

    private static string Encoding_hex(byte[] data)
    {
        var sb = new StringBuilder(data.Length * 2);
        foreach (var b in data) sb.AppendFormat("{0:x2}", b);
        return sb.ToString();
    }

    private static byte[] Encoding_hex_decode(string hex)
    {
        if (string.IsNullOrEmpty(hex)) return Array.Empty<byte>();
        byte[] result = new byte[hex.Length / 2];
        for (int i = 0; i < result.Length; i++) result[i] = Convert.ToByte(hex.Substring(i * 2, 2), 16);
        return result;
    }
}
