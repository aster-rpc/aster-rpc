namespace Aster.Codec;

/// <summary>
/// Wire serialization indirection. Aster supports multiple modes (raw bytes,
/// Fory cross-language, JSON for tests). The active codec advertises its
/// mode string in the registry lease's <c>serialization_modes</c> list so
/// callers know how to encode requests.
/// </summary>
public interface ICodec
{
    /// <summary>
    /// Mode tag matching the registry contract (e.g. "raw", "fory-xlang").
    /// AsterServer publishes this in its lease so clients can pick a
    /// compatible codec via the standard mandatory filters.
    /// </summary>
    string Mode { get; }

    /// <summary>Encode a value to bytes.</summary>
    byte[] Encode(object? value);

    /// <summary>Decode bytes back to a value of the requested type.</summary>
    object? Decode(byte[] payload, Type type);
}

/// <summary>
/// Pass-through codec: only accepts <c>byte[]</c> values, returns them as-is.
/// Useful for opaque-payload services and tests where the host owns the
/// wire format end-to-end.
/// </summary>
public sealed class RawBytesCodec : ICodec
{
    public string Mode => "raw";

    public byte[] Encode(object? value)
    {
        if (value is null) return Array.Empty<byte>();
        if (value is byte[] bytes) return bytes;
        throw new InvalidOperationException(
            $"RawBytesCodec only accepts byte[]; got {value.GetType()}");
    }

    public object? Decode(byte[] payload, Type type)
    {
        if (type == typeof(byte[])) return payload;
        if (type == typeof(object)) return payload;
        throw new InvalidOperationException(
            $"RawBytesCodec only decodes to byte[]; got {type}");
    }
}
