using System;

namespace Aster.Codec;

/// <summary>
/// Payload serialization for RPC requests and responses. The envelope (stream header,
/// trailer) is AEv1 and never goes through a codec; compression is applied by the framing
/// layer. The mode string is advertised in registry leases (<c>serialization_modes</c>).
/// </summary>
public interface ICodec
{
    /// <summary>Mode tag matching the registry contract (e.g. "fory-xlang", "raw").</summary>
    string Mode { get; }

    /// <summary>Encodes <paramref name="value"/> as a payload.</summary>
    byte[] Encode<T>(T value);

    /// <summary>Decodes a payload as <typeparamref name="T"/>.</summary>
    T Decode<T>(ReadOnlySpan<byte> payload);
}

/// <summary>
/// Pass-through codec for <c>byte[]</c> payloads (opaque-payload services and tests where the
/// host owns the wire format end to end).
/// </summary>
public sealed class RawBytesCodec : ICodec
{
    public string Mode => "raw";

    public byte[] Encode<T>(T value) => value switch
    {
        null => Array.Empty<byte>(),
        byte[] bytes => bytes,
        ReadOnlyMemory<byte> mem => mem.ToArray(),
        _ => throw new InvalidOperationException($"RawBytesCodec only accepts byte[]; got {value.GetType()}"),
    };

    public T Decode<T>(ReadOnlySpan<byte> payload)
    {
        if (typeof(T) == typeof(byte[]) || typeof(T) == typeof(object))
            return (T)(object)payload.ToArray();
        if (typeof(T) == typeof(ReadOnlyMemory<byte>))
            return (T)(object)new ReadOnlyMemory<byte>(payload.ToArray());
        throw new InvalidOperationException($"RawBytesCodec only decodes to byte[]; got {typeof(T)}");
    }
}
