using System;
using Apache.Fory;

namespace Aster.Codec;

/// <summary>
/// Apache Fory 1.7.6 codec for RPC payloads, configured as Aster-SPEC §5.1 requires of every
/// binding (Rust <c>xlang::new_fory</c>): cross-language format (Fory C# is always xlang),
/// compatible mode on, reference tracking off. Different options produce different bytes, so
/// they must not drift.
///
/// Backed by a <see cref="ThreadSafeFory"/>, so one codec can serve every in-flight call.
/// Register payload types (classes marked <c>[ForyStruct]</c>) with their Aster wire tags via
/// <see cref="Register{T}(string)"/> before the first call.
/// </summary>
public sealed class ForyCodec : ICodec, IDisposable
{
    /// <summary>Pinned Apache Fory release (Aster-SPEC §5.1; Rust <c>xlang::FORY_VERSION</c>).</summary>
    public const string ForyVersion = "1.7.6";

    private readonly ThreadSafeFory _fory;

    public ForyCodec() : this(NewBuilder().BuildThreadSafe()) { }

    /// <summary>Wraps a caller-built instance (it must use <see cref="NewBuilder"/>'s options).</summary>
    public ForyCodec(ThreadSafeFory fory) => _fory = fory;

    /// <summary>A builder preset to Aster's options: compatible mode on, reference tracking off.</summary>
    public static ForyBuilder NewBuilder() => Apache.Fory.Fory.Builder().Compatible(true).TrackRef(false);

    /// <summary>The underlying Fory instance (e.g. for custom serializer registration).</summary>
    public ThreadSafeFory Fory => _fory;

    public string Mode => "fory-xlang";

    /// <summary>
    /// Registers <typeparamref name="T"/> under an Aster wire tag such as
    /// <c>"interop/EchoRequest"</c>: the namespace is everything before the last <c>/</c>, the
    /// type name the rest (the same split every binding uses, so the bytes match). A tag
    /// without <c>/</c> registers with an empty namespace.
    /// </summary>
    public ForyCodec Register<T>(string wireTag)
    {
        ArgumentException.ThrowIfNullOrEmpty(wireTag);
        int slash = wireTag.LastIndexOf('/');
        if (slash >= 0)
            _fory.Register<T>(wireTag[..slash], wireTag[(slash + 1)..]);
        else
            _fory.Register<T>(string.Empty, wireTag);
        return this;
    }

    /// <summary>Registers <typeparamref name="T"/> under a numeric Fory type id.</summary>
    public ForyCodec Register<T>(uint typeId)
    {
        _fory.Register<T>(typeId);
        return this;
    }

    /// <summary>
    /// Encodes <paramref name="value"/>. The contract-type rules run first, in place
    /// (<see cref="Aster.Rpc.ContractValues"/>): timestamps truncate to microseconds, <c>decimal(p,s)</c>
    /// fields rescale to <c>s</c>, and a value that does not fit throws <see cref="Aster.Rpc.ContractValueException"/>.
    /// </summary>
    public byte[] Encode<T>(T value)
    {
        Aster.Rpc.ContractValues.BeforeEncode(value);
        return _fory.Serialize(in value);
    }

    /// <summary>
    /// Decodes a payload, then applies the contract-type rules: <c>decimal(p,s)</c> fields are rescaled to
    /// <c>s</c> and <c>uuid</c> / <c>ulid</c> fields must be 16 bytes, else <see cref="Aster.Rpc.ContractValueException"/>.
    /// </summary>
    public T Decode<T>(ReadOnlySpan<byte> payload)
    {
        if (payload.IsEmpty)
            throw new InvalidOperationException($"empty payload cannot be decoded as {typeof(T).Name}");
        T value = _fory.Deserialize<T>(payload);
        Aster.Rpc.ContractValues.AfterDecode(value);
        return value;
    }

    public void Dispose() => _fory.Dispose();
}
