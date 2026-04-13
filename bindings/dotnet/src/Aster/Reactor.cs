using System;
using System.Runtime.InteropServices;

namespace Aster;

/// <summary>
/// Incoming RPC call delivered by the reactor.
/// </summary>
public sealed class ReactorCall
{
    public ulong CallId { get; init; }
    public byte[] Header { get; init; } = Array.Empty<byte>();
    public byte HeaderFlags { get; init; }
    public byte[] Request { get; init; } = Array.Empty<byte>();
    public byte RequestFlags { get; init; }
    public string PeerId { get; init; } = string.Empty;
    public bool IsSession { get; init; }
}

/// <summary>
/// Response to an Aster RPC call.
/// </summary>
public sealed class ReactorResponse
{
    public byte[] ResponseFrame { get; init; } = Array.Empty<byte>();
    public byte[] TrailerFrame { get; init; } = Array.Empty<byte>();

    public static ReactorResponse Of(byte[] response) => new() { ResponseFrame = response };
    public static ReactorResponse Of(byte[] response, byte[] trailer) => new() { ResponseFrame = response, TrailerFrame = trailer };
}

/// <summary>
/// Wraps the aster_reactor_* C API for RPC call delivery via SPSC ring buffer.
/// </summary>
public sealed class Reactor : IDisposable
{
    // aster_reactor_call_t is 80 bytes (verified by Rust layout)
    internal const int CallStructSize = 80;

    // Field offsets within aster_reactor_call_t
    private const int OFF_CALL_ID = 0;
    private const int OFF_HEADER_PTR = 8;
    private const int OFF_HEADER_LEN = 16;
    private const int OFF_HEADER_FLAGS = 20;
    private const int OFF_REQUEST_PTR = 24;
    private const int OFF_REQUEST_LEN = 32;
    private const int OFF_REQUEST_FLAGS = 36;
    private const int OFF_PEER_PTR = 40;
    private const int OFF_PEER_LEN = 48;
    private const int OFF_IS_SESSION = 52;
    private const int OFF_HEADER_BUF = 56;
    private const int OFF_REQUEST_BUF = 64;
    private const int OFF_PEER_BUF = 72;

    private readonly ulong _runtimeHandle;
    private readonly ulong _handle;
    private bool _disposed;

    public Reactor(ulong runtimeHandle, ulong nodeHandle, uint ringCapacity = 256)
    {
        _runtimeHandle = runtimeHandle;
        int r = Native.aster_reactor_create(runtimeHandle, nodeHandle, ringCapacity, out _handle);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_reactor_create");
    }

    /// <summary>
    /// Polls for incoming calls. Returns calls with data already copied; buffers released.
    /// </summary>
    public ReactorCall[] Poll(int maxCalls = 32, uint timeoutMs = 100)
    {
        IntPtr buf = Marshal.AllocHGlobal(CallStructSize * maxCalls);
        try
        {
            uint n = Native.aster_reactor_poll(_runtimeHandle, _handle, buf, (uint)maxCalls, timeoutMs);
            if (n == 0) return Array.Empty<ReactorCall>();

            var calls = new ReactorCall[n];
            for (int i = 0; i < (int)n; i++)
            {
                IntPtr slot = IntPtr.Add(buf, i * CallStructSize);
                calls[i] = ExtractCall(slot);

                // Release native buffers after copy
                ulong headerBuf = (ulong)Marshal.ReadInt64(slot, OFF_HEADER_BUF);
                ulong requestBuf = (ulong)Marshal.ReadInt64(slot, OFF_REQUEST_BUF);
                ulong peerBuf = (ulong)Marshal.ReadInt64(slot, OFF_PEER_BUF);
                Native.aster_reactor_buffer_release(_runtimeHandle, _handle, headerBuf);
                Native.aster_reactor_buffer_release(_runtimeHandle, _handle, requestBuf);
                Native.aster_reactor_buffer_release(_runtimeHandle, _handle, peerBuf);
            }
            return calls;
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    /// <summary>Submits a response for a call.</summary>
    public void Submit(ulong callId, ReactorResponse response)
    {
        IntPtr respPtr = IntPtr.Zero;
        IntPtr trailerPtr = IntPtr.Zero;
        GCHandle respPin = default;
        GCHandle trailerPin = default;

        try
        {
            uint respLen = 0;
            if (response.ResponseFrame.Length > 0)
            {
                respPin = GCHandle.Alloc(response.ResponseFrame, GCHandleType.Pinned);
                respPtr = respPin.AddrOfPinnedObject();
                respLen = (uint)response.ResponseFrame.Length;
            }

            uint trailerLen = 0;
            if (response.TrailerFrame.Length > 0)
            {
                trailerPin = GCHandle.Alloc(response.TrailerFrame, GCHandleType.Pinned);
                trailerPtr = trailerPin.AddrOfPinnedObject();
                trailerLen = (uint)response.TrailerFrame.Length;
            }

            int r = Native.aster_reactor_submit(_runtimeHandle, _handle, callId, respPtr, respLen, trailerPtr, trailerLen);
            if (r != 0)
                throw IrohException.FromStatus(r, "aster_reactor_submit");
        }
        finally
        {
            if (respPin.IsAllocated) respPin.Free();
            if (trailerPin.IsAllocated) trailerPin.Free();
        }
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        Native.aster_reactor_destroy(_runtimeHandle, _handle);
    }

    private static ReactorCall ExtractCall(IntPtr slot)
    {
        ulong callId = (ulong)Marshal.ReadInt64(slot, OFF_CALL_ID);
        IntPtr headerPtr = Marshal.ReadIntPtr(slot, OFF_HEADER_PTR);
        int headerLen = Marshal.ReadInt32(slot, OFF_HEADER_LEN);
        byte headerFlags = Marshal.ReadByte(slot, OFF_HEADER_FLAGS);
        IntPtr requestPtr = Marshal.ReadIntPtr(slot, OFF_REQUEST_PTR);
        int requestLen = Marshal.ReadInt32(slot, OFF_REQUEST_LEN);
        byte requestFlags = Marshal.ReadByte(slot, OFF_REQUEST_FLAGS);
        IntPtr peerPtr = Marshal.ReadIntPtr(slot, OFF_PEER_PTR);
        int peerLen = Marshal.ReadInt32(slot, OFF_PEER_LEN);
        byte isSession = Marshal.ReadByte(slot, OFF_IS_SESSION);

        byte[] header = headerLen > 0 ? new byte[headerLen] : Array.Empty<byte>();
        if (headerLen > 0) Marshal.Copy(headerPtr, header, 0, headerLen);

        byte[] request = requestLen > 0 ? new byte[requestLen] : Array.Empty<byte>();
        if (requestLen > 0) Marshal.Copy(requestPtr, request, 0, requestLen);

        string peerId = string.Empty;
        if (peerLen > 0)
        {
            byte[] peerBytes = new byte[peerLen];
            Marshal.Copy(peerPtr, peerBytes, 0, peerLen);
            peerId = System.Text.Encoding.UTF8.GetString(peerBytes);
        }

        return new ReactorCall
        {
            CallId = callId,
            Header = header,
            HeaderFlags = headerFlags,
            Request = request,
            RequestFlags = requestFlags,
            PeerId = peerId,
            IsSession = isSession != 0,
        };
    }
}
