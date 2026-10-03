using System;
using System.Runtime.InteropServices;
using Aster;

namespace Aster.Tests;

/// <summary>
/// ABI 3.0 handshake and reactor smoke tests against the real native library.
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
    public async System.Threading.Tasks.Task Reactor_Poll_Times_Out_Empty_And_Rejects_Unknown_Submit()
    {
        using var node = await Node.MemoryWithAlpnsAsync(new[] { AsterServer.AsterAlpn });
        using var reactor = new Reactor(node.Runtime.Handle, node.Handle.Value, 64);
        var calls = reactor.Poll(8, 10);
        Assert.Empty(calls);

        // Unknown call id: must surface an error (not silently drop / spin).
        var ex = Assert.Throws<IrohException>(() =>
            reactor.Submit(0xdead_beef, ReactorResponse.Of(new byte[] { 1, 2, 3 })));
        Assert.NotEqual(Status.OK, ex.Status);
        Assert.NotEqual(Status.QueueFull, ex.Status);
    }
}
