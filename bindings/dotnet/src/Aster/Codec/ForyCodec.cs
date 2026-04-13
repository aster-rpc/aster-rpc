using Apache.Fory;

namespace Aster.Codec;

/// <summary>
/// Apache Fory v0.16 backed codec. Exposes the underlying <see cref="Fory"/>
/// instance so the host (or eventually a decorator-driven generator) can
/// register the contract types it needs to serialize. Type registration is
/// the caller's responsibility — this class only owns the encode/decode
/// pump and the mode tag the registry advertises.
/// </summary>
public sealed class ForyCodec : ICodec
{
    private readonly Fory _fory;

    public ForyCodec()
    {
        _fory = Fory.Builder().Build();
    }

    public ForyCodec(Fory fory)
    {
        _fory = fory;
    }

    /// <summary>
    /// The underlying Fory instance. Use this to register types via
    /// <c>fory.Register&lt;T&gt;(typeId)</c> before serializing them.
    /// </summary>
    public Fory Fory => _fory;

    public string Mode => "fory-xlang";

    public byte[] Encode(object? value)
    {
        if (value is null) return Array.Empty<byte>();
        return _fory.Serialize(value);
    }

    public object? Decode(byte[] payload, Type type)
    {
        if (payload.Length == 0) return null;
        // Apache.Fory dispatches the typed deserialize via the runtime; the
        // generic-parameter overload ensures the payload header type id and
        // the requested CLR type line up.
        return typeof(Fory)
            .GetMethod(nameof(Fory.Deserialize), new[] { typeof(byte[]) })
            ?.MakeGenericMethod(type)
            .Invoke(_fory, new object[] { payload });
    }
}
