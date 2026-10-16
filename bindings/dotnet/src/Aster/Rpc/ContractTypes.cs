using System;
using System.Collections;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Numerics;
using System.Reflection;
using System.Security.Cryptography;
using Apache.Fory;
using Aster.Interceptors;

namespace Aster.Rpc;

// Canonical contract types (Aster-ContractIdentity.md, "Time, UUID, and Decimal"):
//
//   contract type   .NET field type        wire (Fory xlang)
//   timestamp       DateTimeOffset         TIMESTAMP (38), truncated to microseconds on encode
//   date            DateOnly               DATE (39)
//   decimal         ForyDecimal            DECIMAL (40), arbitrary precision
//   decimal(p,s)    ForyDecimal + [WireDecimal(p, s)]   DECIMAL (40) at scale s, checked both ways
//   uuid            byte[] + [WireUuid]    BINARY (41) of exactly 16 bytes, RFC 9562 order
//   ulid            byte[] + [WireUlid]    BINARY (41) of exactly 16 bytes
//
// Fory C# 1.7 puts only these field types on the wire with the type ids Rust and Python
// use (it has no xlang UUID, and no serializer for System.Decimal), so the payload fields
// are the wire types and AsterUuid / Ulid / AsterDecimal convert to the .NET types. The
// ForyCodec applies the encode / decode rules below to every payload.

/// <summary>
/// Marks a <c>byte[]</c> field (or the <c>byte[]</c> elements / map keys of a container field) as
/// the contract type <c>uuid</c>: 16 raw bytes in RFC 9562 (big-endian) order. Convert a
/// <see cref="Guid"/> with <see cref="AsterUuid"/>, never <see cref="Guid.ToByteArray()"/>. The
/// codec refuses any other length on encode and decode.
/// </summary>
[AttributeUsage(AttributeTargets.Property | AttributeTargets.Field, Inherited = false)]
public sealed class WireUuidAttribute : Attribute;

/// <summary>
/// Marks a <c>byte[]</c> field (or the <c>byte[]</c> elements / map keys of a container field) as
/// the contract type <c>ulid</c>: the same 16-byte wire as <c>uuid</c> with its own identity and a
/// Crockford base32 string form (<see cref="Ulid"/>). The codec refuses any other length.
/// </summary>
[AttributeUsage(AttributeTargets.Property | AttributeTargets.Field, Inherited = false)]
public sealed class WireUlidAttribute : Attribute;

/// <summary>
/// Constrains a <see cref="ForyDecimal"/> field (or its decimal elements / map values) to the
/// contract type <c>decimal(p,s)</c> (Rust <c>FixedDecimal&lt;P, S&gt;</c>): precision
/// <paramref name="precision"/> (1..1000 significant digits of the unscaled value) at scale
/// <paramref name="scale"/> (0..p). The codec rescales each value to scale exactly <c>s</c> on encode
/// and on decode, padding with zeros, and refuses a value that would need rounding or has more than
/// <c>p</c> digits: never rounds or truncates.
/// </summary>
[AttributeUsage(AttributeTargets.Property | AttributeTargets.Field, Inherited = false)]
public sealed class WireDecimalAttribute : Attribute
{
    public WireDecimalAttribute(int precision, int scale)
    {
        if (precision < 1 || precision > 1000)
            throw new ArgumentOutOfRangeException(nameof(precision), "decimal(p,s) precision must be 1..1000");
        if (scale < 0 || scale > precision)
            throw new ArgumentOutOfRangeException(nameof(scale), "decimal(p,s) scale must be 0..p");
        Precision = precision;
        Scale = scale;
    }

    public int Precision { get; }
    public int Scale { get; }

    /// <summary>The contract <c>type_primitive</c>, e.g. <c>decimal(18,4)</c>.</summary>
    public string Primitive => $"decimal({Precision},{Scale})";
}

/// <summary>A payload value that does not fit its contract type (a <c>decimal(p,s)</c>, <c>uuid</c> or <c>ulid</c> rule).</summary>
public sealed class ContractValueException(string message) : RpcError(StatusCode.InvalidArgument, message);

/// <summary>The contract type <c>uuid</c>: <see cref="Guid"/> to and from its 16 RFC 9562 (big-endian) bytes.</summary>
public static class AsterUuid
{
    /// <summary>The 16 wire bytes of <paramref name="value"/> in RFC 9562 order (not <see cref="Guid.ToByteArray()"/>'s mixed-endian order).</summary>
    public static byte[] ToBytes(Guid value) => value.ToByteArray(bigEndian: true);

    /// <summary>The <see cref="Guid"/> of 16 RFC 9562-order bytes; any other length throws <see cref="ContractValueException"/>.</summary>
    public static Guid ToGuid(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length != 16) throw new ContractValueException($"uuid must be 16 bytes, got {bytes.Length}");
        return new Guid(bytes, bigEndian: true);
    }

    /// <summary>The canonical string form: lowercase hyphenated <c>8-4-4-4-12</c>.</summary>
    public static string Format(ReadOnlySpan<byte> bytes) => ToGuid(bytes).ToString("D");
}

/// <summary>
/// The contract type <c>ulid</c>: 16 bytes (48-bit big-endian millisecond timestamp, then 80 random
/// bits) with a 26-character Crockford base32 string form, upper case on output and case-insensitive
/// on input.
/// </summary>
public readonly struct Ulid : IEquatable<Ulid>, IComparable<Ulid>
{
    private const string Alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    private readonly UInt128 _value;

    private Ulid(UInt128 value) => _value = value;

    /// <summary>The ULID of 16 wire bytes; any other length throws <see cref="ContractValueException"/>.</summary>
    public static Ulid FromBytes(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length != 16) throw new ContractValueException($"ulid must be 16 bytes, got {bytes.Length}");
        return new(System.Buffers.Binary.BinaryPrimitives.ReadUInt128BigEndian(bytes));
    }

    /// <summary>A new ULID at <paramref name="time"/> (default: now) with 80 random bits.</summary>
    public static Ulid NewUlid(DateTimeOffset? time = null)
    {
        long ms = (time ?? DateTimeOffset.UtcNow).ToUnixTimeMilliseconds();
        if (ms < 0 || ms >= 1L << 48) throw new ArgumentOutOfRangeException(nameof(time), "ULID time must fit 48 bits of milliseconds");
        Span<byte> b = stackalloc byte[16];
        RandomNumberGenerator.Fill(b[6..]);
        for (int i = 5; i >= 0; i--, ms >>= 8) b[i] = (byte)ms;
        return FromBytes(b);
    }

    /// <summary>Parses the 26-character Crockford base32 form (case-insensitive; first character 0–7).</summary>
    public static Ulid Parse(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        if (text.Length != 26) throw new FormatException("ulid must be 26 Crockford base32 characters");
        UInt128 v = 0;
        for (int i = 0; i < 26; i++)
        {
            int d = Alphabet.IndexOf(char.ToUpperInvariant(text[i]));
            if (d < 0) throw new FormatException($"invalid ulid character '{text[i]}'");
            if (i == 0 && d > 7) throw new FormatException("ulid first character must be 0-7");
            v = (v << 5) | (uint)d;
        }
        return new(v);
    }

    /// <summary>The 16 wire bytes.</summary>
    public byte[] ToBytes()
    {
        var b = new byte[16];
        System.Buffers.Binary.BinaryPrimitives.WriteUInt128BigEndian(b, _value);
        return b;
    }

    /// <summary>The millisecond timestamp of the first 48 bits.</summary>
    public DateTimeOffset Time => DateTimeOffset.FromUnixTimeMilliseconds((long)(ulong)(_value >> 80));

    public override string ToString()
    {
        Span<char> c = stackalloc char[26];
        UInt128 v = _value;
        for (int i = 25; i >= 0; i--, v >>= 5) c[i] = Alphabet[(int)(uint)(v & 31)];
        return new string(c);
    }

    public bool Equals(Ulid other) => _value == other._value;
    public override bool Equals(object? obj) => obj is Ulid u && Equals(u);
    public override int GetHashCode() => _value.GetHashCode();
    public int CompareTo(Ulid other) => _value.CompareTo(other._value);
    public static bool operator ==(Ulid a, Ulid b) => a.Equals(b);
    public static bool operator !=(Ulid a, Ulid b) => !a.Equals(b);
}

/// <summary>
/// The contract types <c>decimal</c> and <c>decimal(p,s)</c> on Fory's arbitrary-precision
/// <see cref="ForyDecimal"/>: exact conversion to and from <see cref="decimal"/> (which holds a
/// 96-bit unscaled value at scale 0..28, so a wider value is refused rather than rounded), the
/// <c>decimal(p,s)</c> rescale rule, and the decimal string form.
/// </summary>
public static class AsterDecimal
{
    private static readonly BigInteger DecimalMantissaMax = (BigInteger.One << 96) - 1;

    /// <summary>The exact <see cref="ForyDecimal"/> of <paramref name="value"/>, keeping its scale (trailing zeros included).</summary>
    public static ForyDecimal FromDecimal(decimal value)
    {
        Span<int> bits = stackalloc int[4];
        decimal.GetBits(value, bits);
        var mantissa = new BigInteger((uint)bits[0]) | (new BigInteger((uint)bits[1]) << 32) | (new BigInteger((uint)bits[2]) << 64);
        int scale = (bits[3] >> 16) & 0xff;
        return new ForyDecimal(bits[3] < 0 ? -mantissa : mantissa, scale);
    }

    /// <summary>
    /// The exact <see cref="decimal"/> of <paramref name="value"/>. Trailing zeros are dropped only as
    /// far as needed to fit; a value that <see cref="decimal"/> cannot hold exactly (more than 96 bits of
    /// unscaled value, or more than 28 significant decimal places) throws <see cref="OverflowException"/>.
    /// </summary>
    public static decimal ToDecimal(ForyDecimal value)
    {
        if (!TryToDecimal(value, out decimal result))
            throw new OverflowException($"decimal value {Format(value)} does not fit System.Decimal (96-bit unscaled, scale 0..28) without rounding");
        return result;
    }

    /// <summary><see cref="ToDecimal"/> without throwing.</summary>
    public static bool TryToDecimal(ForyDecimal value, out decimal result)
    {
        result = default;
        BigInteger u = value.UnscaledValue;
        long scale = value.Scale;
        if (scale < 0)
        {
            if (scale < -60) return u.IsZero;
            u *= BigInteger.Pow(10, (int)-scale);
            scale = 0;
        }
        while (scale > 28 || BigInteger.Abs(u) > DecimalMantissaMax)
        {
            if (scale == 0) return false;
            var q = BigInteger.DivRem(u, 10, out var rem);
            if (!rem.IsZero) return false;
            u = q;
            scale--;
        }
        bool negative = u.Sign < 0;
        var m = BigInteger.Abs(u);
        result = new decimal((int)(uint)(m & uint.MaxValue), (int)(uint)((m >> 32) & uint.MaxValue),
            (int)(uint)((m >> 64) & uint.MaxValue), negative, (byte)scale);
        return true;
    }

    /// <summary>
    /// The <c>decimal(p,s)</c> rule: <paramref name="value"/> at scale exactly <paramref name="scale"/>,
    /// zero-padded; throws <see cref="ContractValueException"/> when that needs rounding or leaves more
    /// than <paramref name="precision"/> digits.
    /// </summary>
    public static ForyDecimal Rescale(ForyDecimal value, int precision, int scale)
    {
        BigInteger u = value.UnscaledValue;
        long have = value.Scale;
        if (have < scale)
            u *= BigInteger.Pow(10, checked((int)(scale - have)));
        else if (have > scale)
        {
            var q = BigInteger.DivRem(u, BigInteger.Pow(10, checked((int)(have - scale))), out var rem);
            if (!rem.IsZero)
                throw new ContractValueException($"decimal {Format(value)} needs rounding to fit decimal({precision},{scale})");
            u = q;
        }
        if (Digits(u) > precision)
            throw new ContractValueException($"decimal {Format(value)} has more than {precision} digits at scale {scale}");
        return new ForyDecimal(u, scale);
    }

    private static int Digits(BigInteger u)
    {
        u = BigInteger.Abs(u);
        return u.IsZero ? 1 : u.ToString(System.Globalization.CultureInfo.InvariantCulture).Length;
    }

    /// <summary>The plain decimal string form (e.g. <c>-12345.6700</c>; the JSON form of the contract types).</summary>
    public static string Format(ForyDecimal value)
    {
        var inv = System.Globalization.CultureInfo.InvariantCulture;
        BigInteger u = value.UnscaledValue;
        string digits = BigInteger.Abs(u).ToString(inv);
        string sign = u.Sign < 0 ? "-" : "";
        if (value.Scale <= 0) return sign + digits + (u.IsZero ? "" : new string('0', -value.Scale));
        if (digits.Length <= value.Scale) digits = new string('0', value.Scale - digits.Length + 1) + digits;
        return sign + digits[..^value.Scale] + "." + digits[^value.Scale..];
    }

    /// <summary>Parses a plain decimal string (<c>[-+]digits[.digits]</c>) at its written scale.</summary>
    public static ForyDecimal Parse(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        string s = text.Trim();
        int dot = s.IndexOf('.');
        string digits = dot < 0 ? s : s.Remove(dot, 1);
        int scale = dot < 0 ? 0 : s.Length - dot - 1;
        if (!BigInteger.TryParse(digits, System.Globalization.NumberStyles.AllowLeadingSign, System.Globalization.CultureInfo.InvariantCulture, out var u))
            throw new FormatException($"invalid decimal '{text}'");
        return new ForyDecimal(u, scale);
    }
}

/// <summary>
/// Applies the contract-type rules to a payload object graph, around Fory (ForyCodec):
/// before encode, timestamps are truncated to microseconds, <c>decimal(p,s)</c> values are
/// rescaled to <c>s</c>, and <c>uuid</c> / <c>ulid</c> lengths are checked; after decode, the same
/// rescale and length checks run. A value that does not fit throws
/// <see cref="ContractValueException"/>. Types with none of these fields cost one cached lookup.
/// </summary>
internal static class ContractValues
{
    private enum Leaf { None, Timestamp, Fixed, Id16, Struct }

    private sealed record Slot(Func<object, object?> Get, Action<object, object?>? Set, Leaf Leaf, Leaf KeyLeaf,
        WireDecimalAttribute? Fixed, string IdName, Type? Nested, string Name);

    private sealed class Plan(Slot[] slots) { public Slot[] Slots { get; } = slots; }

    private static readonly ConcurrentDictionary<Type, Plan> s_plans = new();
    private static readonly Plan Empty = new([]);

    public static void BeforeEncode<T>(T value)
    {
        if (value is null) return;
        Walk(value, encode: true, depth: 0);
    }

    public static void AfterDecode<T>(T value)
    {
        if (value is null) return;
        Walk(value, encode: false, depth: 0);
    }

    private static void Walk(object obj, bool encode, int depth)
    {
        if (depth > 512) return;
        var plan = PlanFor(obj.GetType());
        foreach (var slot in plan.Slots)
        {
            object? v = slot.Get(obj);
            if (v is null) continue;
            switch (v)
            {
                case IDictionary dict:
                    if (slot.KeyLeaf != Leaf.None)
                        foreach (var key in dict.Keys) Check(slot, slot.KeyLeaf, key, encode, depth);
                    if (slot.Leaf != Leaf.None)
                        foreach (var key in dict.Keys.Cast<object>().ToList())
                        {
                            object? item = dict[key];
                            object? fixedItem = Apply(slot, slot.Leaf, item, encode, depth);
                            if (!ReferenceEquals(fixedItem, item) && !Equals(fixedItem, item)) dict[key] = fixedItem;
                        }
                    break;
                case IList list when v is not byte[]:
                    for (int i = 0; i < list.Count; i++)
                    {
                        object? item = list[i];
                        object? fixedItem = Apply(slot, slot.Leaf, item, encode, depth);
                        if (!ReferenceEquals(fixedItem, item) && !Equals(fixedItem, item)) list[i] = fixedItem;
                    }
                    break;
                case IEnumerable set when v is not byte[] and not string:
                    // Sets: check only (rewriting a hashed member would need a rebuild).
                    foreach (var item in set) Check(slot, slot.Leaf, item, encode, depth);
                    break;
                default:
                {
                    object? fixedValue = Apply(slot, slot.Leaf, v, encode, depth);
                    if (slot.Set is not null && !ReferenceEquals(fixedValue, v) && !Equals(fixedValue, v)) slot.Set(obj, fixedValue);
                    break;
                }
            }
        }
    }

    private static void Check(Slot slot, Leaf leaf, object? item, bool encode, int depth)
    {
        object? fixedItem = Apply(slot, leaf, item, encode, depth);
        if (!Equals(fixedItem, item))
            throw new ContractValueException($"{slot.Name}: a set member or map key must already fit its contract type");
    }

    private static object? Apply(Slot slot, Leaf leaf, object? v, bool encode, int depth)
    {
        if (v is null) return null;
        switch (leaf)
        {
            case Leaf.Timestamp when encode && v is DateTimeOffset t:
                long extra = t.Ticks % 10;
                return extra == 0 ? v : t.AddTicks(-extra);
            case Leaf.Fixed when v is ForyDecimal d:
                var r = AsterDecimal.Rescale(d, slot.Fixed!.Precision, slot.Fixed.Scale);
                return r == d ? v : r;
            case Leaf.Id16 when v is byte[] b:
                if (b.Length != 16) throw new ContractValueException($"{slot.Name}: {slot.IdName} must be 16 bytes, got {b.Length}");
                return v;
            case Leaf.Struct:
                Walk(v, encode, depth + 1);
                return v;
            default:
                return v;
        }
    }

    private static Plan PlanFor(Type type) => s_plans.TryGetValue(type, out var p) ? p : Build(type, new HashSet<Type>());

    private static Plan Build(Type type, HashSet<Type> visiting)
    {
        if (s_plans.TryGetValue(type, out var cached)) return cached;
        if (type.GetCustomAttribute<WireTypeAttribute>() is null)
            return s_plans.GetOrAdd(type, Empty);
        if (!visiting.Add(type)) return Empty; // in progress: the caller keeps the edge (Leaf.Struct)
        var slots = new List<Slot>();
        foreach (var member in ContractBuilder.PayloadMembers(type))
        {
            Type t = member is PropertyInfo pi ? pi.PropertyType : ((FieldInfo)member).FieldType;
            var (valueType, keyType) = ContractBuilder.ContainerTypes(t);
            var fixedAttr = member.GetCustomAttribute<WireDecimalAttribute>();
            string idName = member.GetCustomAttribute<WireUuidAttribute>() is not null ? "uuid"
                : member.GetCustomAttribute<WireUlidAttribute>() is not null ? "ulid" : "";
            Leaf LeafOf(Type? x)
            {
                if (x is null) return Leaf.None;
                x = Nullable.GetUnderlyingType(x) ?? x;
                if (x == typeof(DateTimeOffset)) return Leaf.Timestamp;
                if (x == typeof(ForyDecimal)) return fixedAttr is null ? Leaf.None : Leaf.Fixed;
                if (x == typeof(byte[])) return idName.Length == 0 ? Leaf.None : Leaf.Id16;
                if (x.IsClass && x != typeof(string) && x.GetCustomAttribute<WireTypeAttribute>() is not null)
                {
                    var nested = Build(x, visiting);
                    return nested.Slots.Length > 0 || visiting.Contains(x) ? Leaf.Struct : Leaf.None;
                }
                return Leaf.None;
            }
            Leaf leaf = LeafOf(valueType), keyLeaf = LeafOf(keyType);
            if (leaf == Leaf.None && keyLeaf == Leaf.None) continue;
            Func<object, object?> get = member is PropertyInfo p ? p.GetValue : ((FieldInfo)member).GetValue;
            Action<object, object?>? set = member is PropertyInfo ps ? (ps.CanWrite ? ps.SetValue : null) : ((FieldInfo)member).SetValue;
            slots.Add(new Slot(get, set, leaf, keyLeaf, fixedAttr, idName, valueType, $"{type.Name}.{member.Name}"));
        }
        visiting.Remove(type);
        var plan = new Plan(slots.ToArray());
        // An in-progress cycle may have seen Empty for this type; only publish complete plans.
        return visiting.Count == 0 ? s_plans.GetOrAdd(type, plan) : plan;
    }
}
