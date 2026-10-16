using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// A connect/accept view of a node: the endpoint-shaped API (id, connect, accept, close) over a
/// node started with <c>aster_node_start</c>. ABI 4.0 removed bare endpoints
/// (<c>aster_endpoint_create</c>), so <see cref="CreateAsync(Runtime, EndpointConfigBuilder, CancellationToken)"/>
/// starts an in-memory node on <paramref name="runtime"/> with the builder's ALPNs; closing the
/// endpoint closes that node. Use <see cref="Node.StartAsync"/> for the full configuration.
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

    /// <summary>The native handle (an <c>aster_node_t</c>).</summary>
    public EndpointHandle Handle => _handle;

    /// <summary>The runtime that owns this endpoint.</summary>
    public Runtime Runtime => _runtime;

    /// <summary>
    /// Starts an in-memory node on <paramref name="runtime"/> (<c>aster_node_start</c>) that accepts
    /// the builder's ALPNs. The runtime is not owned: dispose the endpoint before the runtime.
    /// </summary>
    public static async Task<Endpoint> CreateAsync(Runtime runtime, EndpointConfigBuilder builder, CancellationToken cancellationToken = default)
    {
        ulong opId = Node.BeginStart(runtime, builder.ToAsterConfig(), gate: null, extraAlpns: null);
        Event ev = await runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.NodeCreated)
            throw EventError.From(runtime, ev, "aster_node_start");
        return new Endpoint(runtime, new EndpointHandle(ev.handle));
    }

    /// <summary>Creates an endpoint with the default builder.</summary>
    public static Task<Endpoint> CreateAsync(Runtime runtime, CancellationToken cancellationToken = default)
        => CreateAsync(runtime, new EndpointConfigBuilder(), cancellationToken);

    /// <summary>Gets this endpoint's node ID as a hex string.</summary>
    public string NodeId()
    {
        IntPtr buf = Marshal.AllocHGlobal(65);
        try
        {
            int r = Native.aster_node_id(_runtime.Handle, _handle.Value, buf, (UIntPtr)65, out UIntPtr len);
            if (r != 0)
                throw IrohException.FromStatus(r, "aster_node_id");
            if (len == UIntPtr.Zero)
                return string.Empty;
            byte[] bytes = new byte[(int)len];
            Marshal.Copy(buf, bytes, 0, (int)len);
            return Encoding.UTF8.GetString(bytes); // native returns the hex id as UTF-8 text
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    /// <summary>
    /// This endpoint's address: id, relay URL and direct addresses (<c>aster_node_addr_info</c>). Direct
    /// addresses can appear shortly after start; pass the result to <see cref="ConnectWithAddrAsync"/>.
    /// </summary>
    public NodeAddrInfo NodeAddr() => Node.ReadNodeAddr(_runtime, _handle.Value);

    /// <summary>Closes this endpoint (and its node) asynchronously.</summary>
    public async Task CloseAsync(CancellationToken cancellationToken = default)
    {
        int r = Native.aster_node_close(_runtime.Handle, _handle.Value, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_node_close");

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
        int r = Native.aster_accept(_runtime.Handle, _handle.Value, user_data: 0, out ulong opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_accept");

        Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.ConnectionAccepted)
            throw new IrohException($"accept: unexpected event {(EventKind)ev.kind}");

        return new Connection(_runtime, new ConnectionHandle(ev.handle));
    }

    /// <summary>Connects to a remote node by node ID (hex) and ALPN.</summary>
    public async Task<Connection> ConnectAsync(string nodeIdHex, string alpn, CancellationToken cancellationToken = default)
    {
        // aster_connect_config_t.node_id is the hex endpoint id as UTF-8 text.
        byte[] nodeIdBytes = Encoding.UTF8.GetBytes(nodeIdHex);
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

                int r = Native.aster_connect(_runtime.Handle, _handle.Value, configPtr, user_data: 0, out ulong opId);
                if (r != 0)
                    throw IrohException.FromStatus(r, "aster_connect");

                Event ev = await _runtime.WaitForAsync(opId, cancellationToken).ConfigureAwait(false);
                if (ev.kind != (uint)EventKind.Connected)
                    throw new IrohException($"connect: unexpected event {(EventKind)ev.kind}");

                return new Connection(_runtime, new ConnectionHandle(ev.handle));
            }
            finally { Marshal.FreeHGlobal(configPtr); }
        }
        finally { nodeIdHandle.Free(); alpnHandle.Free(); }
    }

    /// <summary>Connects to a remote node using its full address info (relay URL and direct addresses).</summary>
    public Task<Connection> ConnectWithAddrAsync(NodeAddrInfo addr, string alpn, CancellationToken cancellationToken = default)
        => Node.ConnectCore(_runtime, _handle.Value, addr, alpn, cancellationToken);

    public void Dispose() { if (!_disposed) Close(); }


}
