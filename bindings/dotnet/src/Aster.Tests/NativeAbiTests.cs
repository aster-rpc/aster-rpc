using System;
using System.Runtime.InteropServices;
using Aster;

namespace Aster.Tests;

/// <summary>
/// ABI 3.0 handshake and dispatch-server smoke tests against the real native library.
/// </summary>
public class NativeAbiTests
{
    [Fact]
    public void Loaded_Library_Is_Abi_Major_3()
    {
        uint v = Native.aster_abi_version();
        Assert.Equal(Native.AbiVersionMajor, v >> 16);
    }

    [Fact]
    public void RuntimeNew_Rejects_Wrong_Abi_Major()
    {
        var cfg = RuntimeConfig.Default;
        int r = Native.aster_runtime_new(Native.AbiVersionMajor - 1, in cfg, out ulong rt);
        Assert.Equal((int)Status.AbiMismatch, r);
        Assert.Equal(0ul, rt);
        var ex = IrohException.FromStatus(r, "aster_runtime_new");
        Assert.Contains("ASTER_STATUS_ABI_MISMATCH", ex.Message);
    }

    [Fact]
    public void RuntimeNew_Rejects_Wrong_StructSize()
    {
        var cfg = RuntimeConfig.Default;
        cfg.struct_size = 8;
        int r = Native.aster_runtime_new(Native.AbiVersionMajor, in cfg, out _);
        Assert.Equal((int)Status.InvalidArgument, r);
    }

    [Fact]
    public void StatusName_Handles_Unknown_Values()
    {
        string? name = Marshal.PtrToStringUTF8(Native.aster_status_name(-12345));
        Assert.Equal("ASTER_STATUS_UNKNOWN", name);
        Assert.Equal("ASTER_STATUS_QUEUE_FULL", Marshal.PtrToStringUTF8(Native.aster_status_name(4)));
    }

    [Fact]
    public async System.Threading.Tasks.Task DispatchServer_Polls_Empty_And_Reports_Unknown_Calls_As_Over()
    {
        await using var node = await Node.StartAsync(new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" },
            null, [AsterServer.AsterAlpn]);
        DispatchServer server;
        using (var b = new DispatchServer.Builder(node.Runtime.Handle, node.Handle.Value))
        {
            Assert.Equal(0u, b.AddServiceJson(RpcTests.InteropEcho("dotnet").RegistrationJson));
            // A duplicate (name, version) and unknown options are refused.
            Assert.Throws<IrohException>(() => b.AddServiceJson(RpcTests.InteropEcho("dotnet").RegistrationJson));
            Assert.Throws<IrohException>(() => b.Option((DispatchServerOption)99, 1));
            server = b.Start(64);
        }
        using (server)
        {
            unsafe
            {
                var events = stackalloc DispatchEventNative[8];
                Assert.Equal(0u, server.Poll(events, 8, 10));
            }
            // Unknown call id: the call is over (not an error, not QUEUE_FULL).
            Assert.False(await server.RespondAsync(0xdead_beef, new byte[] { 1, 2, 3 }, null));
            Assert.False(await server.FinishAsync(0xdead_beef, null));
            Assert.True(server.IsCancelled(0xdead_beef));
            Assert.Equal(DispatchServer.RecvStatus.Gone, server.Recv(0xdead_beef, 0, out _));
            Assert.Empty(server.Metadata(0xdead_beef));
            Assert.Equal(0ul, server.LiveCalls);
        }
    }
}
