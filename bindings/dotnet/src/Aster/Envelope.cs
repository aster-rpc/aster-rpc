using System;
using System.Collections.Generic;
using System.Linq;
using Aster.Interceptors;

namespace Aster;

/// <summary>Payload serialization modes carried in <see cref="StreamHeader"/> (Aster-SPEC §5.1).</summary>
public enum SerializationMode : sbyte
{
    /// <summary>Fory xlang (the only mode bindings speak; JSON mode 3 is retired).</summary>
    Xlang = 0,
    Native = 1,
    Row = 2,
}

/// <summary>
/// AEv1 <c>StreamHeader</c> (Aster-SPEC §6.9): the HEADER frame that opens every RPC stream.
/// <paramref name="Deadline"/> is relative, in whole seconds (0 = none).
/// </summary>
public sealed record StreamHeader(
    string Service,
    string Method,
    int Version = 1,
    int CallId = 0,
    short Deadline = 0,
    SerializationMode SerializationMode = SerializationMode.Xlang,
    IReadOnlyList<KeyValuePair<string, string>>? Metadata = null,
    int SessionId = 0)
{
    public IReadOnlyList<KeyValuePair<string, string>> Metadata { get; init; } = Metadata ?? Array.Empty<KeyValuePair<string, string>>();
}

/// <summary>AEv1 <c>CallHeader</c>: per-call header inside a session stream.</summary>
public sealed record CallHeader(
    string Method,
    int CallId = 0,
    short Deadline = 0,
    IReadOnlyList<KeyValuePair<string, string>>? Metadata = null)
{
    public IReadOnlyList<KeyValuePair<string, string>> Metadata { get; init; } = Metadata ?? Array.Empty<KeyValuePair<string, string>>();
}

/// <summary>AEv1 <c>RpcStatus</c>: the TRAILER frame payload that ends every call.</summary>
public sealed record RpcStatus(
    StatusCode Code,
    string Message = "",
    IReadOnlyList<KeyValuePair<string, string>>? Details = null)
{
    public IReadOnlyList<KeyValuePair<string, string>> Details { get; init; } = Details ?? Array.Empty<KeyValuePair<string, string>>();

    /// <summary>The OK status.</summary>
    public static RpcStatus Ok { get; } = new(StatusCode.Ok);

    /// <summary>The status for an exception thrown by a handler (INTERNAL unless it is an <see cref="RpcError"/>).</summary>
    public static RpcStatus FromException(Exception e) => e switch
    {
        RpcError rpc => new RpcStatus(rpc.Code, rpc.Message, rpc.Details.ToList()),
        OperationCanceledException => new RpcStatus(StatusCode.Cancelled, e.Message),
        _ => new RpcStatus(StatusCode.Internal, string.IsNullOrEmpty(e.Message) ? e.GetType().Name : e.Message),
    };

    /// <summary>Throws an <see cref="RpcError"/> unless this status is OK.</summary>
    public void ThrowIfError()
    {
        if (Code != StatusCode.Ok)
            throw new RpcError(Code, Message, Details.ToDictionary(kv => kv.Key, kv => kv.Value));
    }
}

/// <summary>
/// AEv1 envelope codec (Aster-SPEC §6.9) through the C ABI (3.1+): the bytes are produced by
/// Rust core, so every binding emits the same envelope. Thread-safe; malformed input throws
/// <see cref="IrohException"/> with <see cref="Status.InvalidArgument"/>.
/// </summary>
public static unsafe class AsterEnvelope
{
    public const byte TagStreamHeader = 0xA1;
    public const byte TagCallHeader = 0xA2;
    public const byte TagRpcStatus = 0xA3;
    public const byte Version = 1;

    public static byte[] Encode(StreamHeader h)
    {
        using var scope = new NativeScope();
        var n = new StreamHeaderNative
        {
            struct_size = (uint)sizeof(StreamHeaderNative),
            version = h.Version,
            call_id = h.CallId,
            session_id = h.SessionId,
            deadline = h.Deadline,
            serialization_mode = (sbyte)h.SerializationMode,
            service = scope.Utf8(h.Service),
            method = scope.Utf8(h.Method),
            metadata_keys = scope.Utf8List(h.Metadata.Select(kv => kv.Key).ToList()),
            metadata_values = scope.Utf8List(h.Metadata.Select(kv => kv.Value).ToList()),
        };
        Bytes output = default;
        Native.Check(Native.aster_stream_header_encode(&n, &output), "aster_stream_header_encode");
        return NativeScope.TakeOwned(ref output);
    }

    public static StreamHeader DecodeStreamHeader(ReadOnlySpan<byte> bytes)
    {
        StreamHeaderNative n = default;
        fixed (byte* p = bytes)
            Native.Check(Native.aster_stream_header_decode(p, (UIntPtr)bytes.Length, &n), "aster_stream_header_decode");
        try
        {
            return new StreamHeader(
                n.service.ToUtf8String(), n.method.ToUtf8String(), n.version, n.call_id, n.deadline,
                (SerializationMode)n.serialization_mode,
                Pairs(n.metadata_keys, n.metadata_values), n.session_id);
        }
        finally { Native.aster_stream_header_release(&n); }
    }

    public static byte[] Encode(CallHeader h)
    {
        using var scope = new NativeScope();
        var n = new CallHeaderNative
        {
            struct_size = (uint)sizeof(CallHeaderNative),
            call_id = h.CallId,
            deadline = h.Deadline,
            method = scope.Utf8(h.Method),
            metadata_keys = scope.Utf8List(h.Metadata.Select(kv => kv.Key).ToList()),
            metadata_values = scope.Utf8List(h.Metadata.Select(kv => kv.Value).ToList()),
        };
        Bytes output = default;
        Native.Check(Native.aster_call_header_encode(&n, &output), "aster_call_header_encode");
        return NativeScope.TakeOwned(ref output);
    }

    public static CallHeader DecodeCallHeader(ReadOnlySpan<byte> bytes)
    {
        CallHeaderNative n = default;
        fixed (byte* p = bytes)
            Native.Check(Native.aster_call_header_decode(p, (UIntPtr)bytes.Length, &n), "aster_call_header_decode");
        try
        {
            return new CallHeader(n.method.ToUtf8String(), n.call_id, n.deadline, Pairs(n.metadata_keys, n.metadata_values));
        }
        finally { Native.aster_call_header_release(&n); }
    }

    public static byte[] Encode(RpcStatus s)
    {
        using var scope = new NativeScope();
        var n = new RpcStatusNative
        {
            struct_size = (uint)sizeof(RpcStatusNative),
            code = (int)s.Code,
            message = scope.Utf8(s.Message),
            detail_keys = scope.Utf8List(s.Details.Select(kv => kv.Key).ToList()),
            detail_values = scope.Utf8List(s.Details.Select(kv => kv.Value).ToList()),
        };
        Bytes output = default;
        Native.Check(Native.aster_rpc_status_encode(&n, &output), "aster_rpc_status_encode");
        return NativeScope.TakeOwned(ref output);
    }

    public static RpcStatus DecodeRpcStatus(ReadOnlySpan<byte> bytes)
    {
        RpcStatusNative n = default;
        fixed (byte* p = bytes)
            Native.Check(Native.aster_rpc_status_decode(p, (UIntPtr)bytes.Length, &n), "aster_rpc_status_decode");
        try
        {
            return new RpcStatus((StatusCode)n.code, n.message.ToUtf8String(), Pairs(n.detail_keys, n.detail_values));
        }
        finally { Native.aster_rpc_status_release(&n); }
    }

    private static List<KeyValuePair<string, string>> Pairs(BytesList keys, BytesList values)
    {
        var k = NativeScope.ReadUtf8List(keys);
        var v = NativeScope.ReadUtf8List(values);
        var result = new List<KeyValuePair<string, string>>(k.Count);
        for (int i = 0; i < Math.Min(k.Count, v.Count); i++) result.Add(new(k[i], v[i]));
        return result;
    }
}

/// <summary>zstd payload compression (Aster-SPEC §5.6) via Rust core (<c>aster_zstd_*</c>).</summary>
public static unsafe class Zstd
{
    /// <summary>Payloads strictly larger than this are compressed (<c>ASTER_COMPRESSION_THRESHOLD</c>).</summary>
    public const int CompressionThreshold = 4096;

    /// <summary>Decompression refuses more output than this (<c>ASTER_MAX_DECOMPRESSED_SIZE</c>).</summary>
    public const int MaxDecompressedSize = 16 * 1024 * 1024;

    /// <summary>
    /// The zstd-compressed payload, or <c>null</c> when it should go uncompressed (at or under
    /// <paramref name="threshold"/>, or compression would not make it smaller).
    /// </summary>
    public static byte[]? MaybeCompress(ReadOnlySpan<byte> payload, int threshold = CompressionThreshold)
    {
        Bytes output = default;
        fixed (byte* p = payload)
            Native.Check(Native.aster_zstd_maybe_compress(p, (UIntPtr)payload.Length, (UIntPtr)threshold, &output),
                "aster_zstd_maybe_compress");
        return output.ptr == IntPtr.Zero ? null : NativeScope.TakeOwned(ref output);
    }

    /// <summary>Decompresses a zstd frame (16 MiB cap); corrupt or over-limit input throws.</summary>
    public static byte[] Decompress(ReadOnlySpan<byte> data)
    {
        Bytes output = default;
        fixed (byte* p = data)
            Native.Check(Native.aster_zstd_decompress(p, (UIntPtr)data.Length, &output), "aster_zstd_decompress");
        return NativeScope.TakeOwned(ref output);
    }
}

/// <summary>
/// Aster wire framing (Aster-SPEC §6.1): <c>[u32 LE body length][u8 flags][payload]</c>,
/// body length = 1 + payload length.
/// </summary>
public static class AsterFraming
{
    public const byte FlagCompressed = 0x01;
    public const byte FlagTrailer = 0x02;
    public const byte FlagHeader = 0x04;
    public const byte FlagRowSchema = 0x08;
    public const byte FlagCall = 0x10;
    public const byte FlagCancel = 0x20;
    /// <summary>Set on the last request frame of a client-stream / bidi call.</summary>
    public const byte FlagEndStream = 0x40;

    /// <summary>Encodes one frame verbatim (envelope frames: HEADER / CALL / TRAILER).</summary>
    public static byte[] EncodeFrame(ReadOnlySpan<byte> payload, byte flags)
    {
        var frame = new byte[5 + payload.Length];
        BitConverter.TryWriteBytes(frame.AsSpan(0, 4), (uint)(1 + payload.Length));
        frame[4] = flags;
        payload.CopyTo(frame.AsSpan(5));
        return frame;
    }

    /// <summary>
    /// Encodes a data frame, zstd-compressing the payload (and setting
    /// <see cref="FlagCompressed"/>) when it exceeds the threshold and compression helps.
    /// </summary>
    public static byte[] EncodePayloadFrame(ReadOnlySpan<byte> payload, byte flags)
    {
        byte[]? compressed = Zstd.MaybeCompress(payload);
        return compressed is not null
            ? EncodeFrame(compressed, (byte)(flags | FlagCompressed))
            : EncodeFrame(payload, (byte)(flags & ~FlagCompressed));
    }

    /// <summary>The logical payload of a received data frame (decompressed when flagged).</summary>
    public static byte[] PayloadOf(byte[] payload, byte flags)
        => (flags & FlagCompressed) != 0 ? Zstd.Decompress(payload) : payload;
}
