using System.Collections;
using System.Globalization;
using System.Reflection;
using System.Text.Json;
using System.Text.Json.Nodes;
using Apache.Fory;
using Aster.Codec;
using Aster.Rpc;

namespace Aster.Tests;

// Mirrors of aster/tests/it/contract_types.rs (the source of conformance/vectors/contract-types.json).

[ForyStruct, WireType("types.v1/Temporal")]
public sealed class CtTemporal
{
    public DateTimeOffset At { get; set; }
    public DateOnly Day { get; set; }
    public ForyDecimal Amount { get; set; }
    public DateTimeOffset? MaybeAt { get; set; }
    public List<DateTimeOffset> History { get; set; } = new();
}

[ForyStruct, WireType("types.v1/TreeNode")]
public sealed class CtTreeNode
{
    public string Value { get; set; } = "";
    public List<CtTreeNode> Children { get; set; } = new();
}

/// <summary>Contract identity only, as in Rust (a cycle through an optional field alone).</summary>
[ForyStruct, WireType("types.v1/Chain")]
public sealed class CtChain
{
    public int Value { get; set; }
    public CtChain? Next { get; set; }
}

[ForyStruct, WireType("types.v1/Author")]
public sealed class CtAuthor
{
    public string Name { get; set; } = "";
    public List<CtBook> Books { get; set; } = new();
}

[ForyStruct, WireType("types.v1/Book")]
public sealed class CtBook
{
    public string Title { get; set; } = "";
    public CtAuthor? WrittenBy { get; set; }
}

[ForyStruct, WireType("types.v1/Alpha")]
public sealed class CtAlpha { public CtBeta? Next { get; set; } }

[ForyStruct, WireType("types.v1/Beta")]
public sealed class CtBeta { public CtGamma? Next { get; set; } }

[ForyStruct, WireType("types.v1/Gamma")]
public sealed class CtGamma { public List<CtAlpha> Next { get; set; } = new(); }

[ForyStruct, WireType("types.v1/Forest")]
public sealed class CtForest { public List<CtTreeNode> Trees { get; set; } = new(); }

[ForyStruct, WireType("types.v1/Identified")]
public sealed class CtIdentified
{
    [WireUuid] public byte[] Id { get; set; } = new byte[16];
    [WireUlid] public byte[] Trace { get; set; } = new byte[16];
    [WireUuid] public byte[]? Parent { get; set; }
    [WireUlid] public List<byte[]> History { get; set; } = new();
    [WireUuid] public Dictionary<byte[], string> Names { get; set; } = new();
}

[ForyStruct, WireType("types.v1/Priced")]
public sealed class CtPriced
{
    [WireDecimal(18, 4)] public ForyDecimal Price { get; set; }
    [WireDecimal(5, 0)] public ForyDecimal Rate { get; set; }
    [WireDecimal(18, 4)] public ForyDecimal? MaybePrice { get; set; }
    public ForyDecimal Amount { get; set; }
}

// A cycle whose spanning tree depends on codepoint (not UTF-16) order: Ａ (U+FF21) and 𝔸
// (U+1D538). Contract identity only, as in Rust.
[ForyStruct, WireType("types.v1/Ord")]
public sealed class CtOrd
{
    public CtWide? Wide { get; set; }
    public CtMath? Math { get; set; }
}

[ForyStruct, WireType("types.v1/\uFF21")]
public sealed class CtWide
{
    public CtOrd? Back { get; set; }
    public CtMath? Math { get; set; }
}

[ForyStruct, WireType("types.v1/\U0001D538")]
public sealed class CtMath { public CtWide? Wide { get; set; } }

/// <summary>
/// conformance/vectors/contract-types.json: the .NET contract types (timestamp, date, decimal,
/// decimal(p,s), uuid, ulid) and recursive types (SELF_REF) give Rust's TypeDef bytes, type hashes
/// and contract ids, decode Rust's payloads to the vector values, and refuse the same decode cases.
/// </summary>
public class ContractTypeVectorTests
{
    private static JsonElement V => Vectors.Load("contract-types.json").GetProperty("vector");

    private static readonly Dictionary<string, Type> Types = new()
    {
        ["types.v1.Temporal"] = typeof(CtTemporal),
        ["types.v1.TreeNode"] = typeof(CtTreeNode),
        ["types.v1.Chain"] = typeof(CtChain),
        ["types.v1.Author"] = typeof(CtAuthor),
        ["types.v1.Book"] = typeof(CtBook),
        ["types.v1.Alpha"] = typeof(CtAlpha),
        ["types.v1.Beta"] = typeof(CtBeta),
        ["types.v1.Gamma"] = typeof(CtGamma),
        ["types.v1.Forest"] = typeof(CtForest),
        ["types.v1.Identified"] = typeof(CtIdentified),
        ["types.v1.Priced"] = typeof(CtPriced),
        ["types.v1.Ord"] = typeof(CtOrd),
        ["types.v1.\uFF21"] = typeof(CtWide),
        ["types.v1.\U0001D538"] = typeof(CtMath),
    };

    private static ForyCodec Codec()
    {
        var codec = new ForyCodec();
        codec.Register<CtTemporal>("types.v1/Temporal").Register<CtTreeNode>("types.v1/TreeNode")
            .Register<CtAuthor>("types.v1/Author").Register<CtBook>("types.v1/Book")
            .Register<CtAlpha>("types.v1/Alpha").Register<CtBeta>("types.v1/Beta").Register<CtGamma>("types.v1/Gamma")
            .Register<CtForest>("types.v1/Forest").Register<CtIdentified>("types.v1/Identified").Register<CtPriced>("types.v1/Priced");
        return codec;
    }

    public static IEnumerable<object[]> TypeNames() => Types.Keys.Select(k => new object[] { k });

    [Theory]
    [MemberData(nameof(TypeNames))]
    public void TypeDef_Bytes_And_Hash_Match_Rust(string name)
    {
        var expected = V.GetProperty("types").GetProperty(name);
        Type type = Types[name];
        byte[] canonical = ContractIdentity.ComputeCanonicalBytes("TypeDef", ContractBuilder.TypeDefJson(type));
        Assert.Equal(expected.Str("expected_canonical_type_def_hex"), Convert.ToHexString(canonical).ToLowerInvariant());
        Assert.Equal(expected.Str("expected_type_hash"), ContractBuilder.TypeHash(type));
    }

    [Fact]
    public void Contract_Ids_Match_Rust()
    {
        var types = ServiceDefinition.Create("Types", 1)
            .Unary<CtTemporal, CtTemporal>("echo_temporal", (r, _) => ValueTask.FromResult(r))
            .Unary<CtTreeNode, CtForest>("plant", (r, _) => ValueTask.FromResult(new CtForest()))
            .Unary<CtBook, CtAuthor>("author", (r, _) => ValueTask.FromResult(new CtAuthor()))
            .Unary<CtAlpha, CtGamma>("cycle", (r, _) => ValueTask.FromResult(new CtGamma()))
            .Build();
        Assert.Equal(V.Str("expected_contract_id"), ContractBuilder.ComputeContractId(types));
        // Rust derives the same id from the registration form.
        Assert.Equal(V.Str("expected_contract_id"), DispatchServer.ContractId(ContractBuilder.RegistrationJson(types)));
        var ids = ServiceDefinition.Create("Ids", 1)
            .Unary<CtIdentified, CtPriced>("price", (r, _) => ValueTask.FromResult(new CtPriced()))
            .Build();
        Assert.Equal(V.Str("expected_ids_contract_id"), ContractBuilder.ComputeContractId(ids));
    }

    public static IEnumerable<object[]> PayloadNames()
        => V.GetProperty("payloads").EnumerateObject().Select(p => new object[] { p.Name });

    // Payloads with no strings: Fory C# writes the same bytes as Rust. With strings it may pick
    // Latin-1 where Rust writes UTF-8 (both decode everywhere), so those compare by value.
    private static readonly HashSet<string> ByteIdentical = ["temporal_small_decimal", "temporal_big_decimal", "priced"];

    [Theory]
    [MemberData(nameof(PayloadNames))]
    public void Rust_Payloads_Decode_To_Vector_Values_And_Round_Trip(string name)
    {
        var payload = V.GetProperty("payloads").GetProperty(name);
        Type type = Types[payload.Str("type")];
        byte[] bytes = payload.Hex("hex");
        using var codec = Codec();
        object decoded = Decode(codec, type, bytes);
        var expected = JsonNode.Parse(payload.GetProperty("value").GetRawText());
        Assert.True(JsonNode.DeepEquals(expected, ToJson(decoded)), $"{name}: {ToJson(decoded)?.ToJsonString()} != {expected?.ToJsonString()}");

        byte[] again = Encode(codec, type, decoded);
        Assert.True(JsonNode.DeepEquals(expected, ToJson(Decode(codec, type, again))), $"{name}: re-encoded value differs");
        if (ByteIdentical.Contains(name))
            Assert.Equal(Convert.ToHexString(bytes), Convert.ToHexString(again));
    }

    public static IEnumerable<object[]> DecodeCaseNames()
        => V.GetProperty("decode_cases").EnumerateObject().Select(p => new object[] { p.Name });

    [Theory]
    [MemberData(nameof(DecodeCaseNames))]
    public void Decode_Cases_Accept_Or_Refuse_Like_Rust(string name)
    {
        var c = V.GetProperty("decode_cases").GetProperty(name);
        Type type = Types[c.Str("type")];
        using var codec = Codec();
        if (c.GetProperty("refused").GetBoolean())
        {
            Assert.Throws<ContractValueException>(() => Decode(codec, type, c.Hex("hex")));
            return;
        }
        var expected = JsonNode.Parse(c.GetProperty("value").GetRawText());
        object decoded = Decode(codec, type, c.Hex("hex"));
        Assert.True(JsonNode.DeepEquals(expected, ToJson(decoded)), $"{name}: {ToJson(decoded)?.ToJsonString()}");
    }

    [Fact]
    public void Encode_Pads_Fixed_Decimals_And_Refuses_What_Does_Not_Fit()
    {
        using var codec = Codec();
        var ok = new CtPriced { Price = AsterDecimal.Parse("1.5"), Rate = AsterDecimal.Parse("7.0"), Amount = AsterDecimal.Parse("0") };
        var back = codec.Decode<CtPriced>(codec.Encode(ok));
        Assert.Equal(new ForyDecimal(15000, 4), back.Price);
        Assert.Equal(new ForyDecimal(7, 0), back.Rate);
        Assert.Throws<ContractValueException>(() => codec.Encode(new CtPriced { Price = AsterDecimal.Parse("1.23456") }));
        Assert.Throws<ContractValueException>(() => codec.Encode(new CtPriced { Rate = AsterDecimal.Parse("123456") }));
        Assert.Throws<ContractValueException>(() => codec.Encode(new CtPriced { Price = AsterDecimal.Parse("123456789012345") }));
        Assert.Throws<ContractValueException>(() => codec.Encode(new CtIdentified { Id = new byte[15] }));
        Assert.Throws<ContractValueException>(() => codec.Encode(new CtIdentified { History = [new byte[17]] }));
    }

    [Fact]
    public void Encode_Truncates_Timestamps_To_Microseconds()
    {
        using var codec = Codec();
        var at = new DateTimeOffset(2024, 3, 1, 12, 34, 56, TimeSpan.Zero).AddTicks(7_890_127);
        var back = codec.Decode<CtTemporal>(codec.Encode(new CtTemporal { At = at, History = [at] }));
        Assert.Equal(at.AddTicks(-7), back.At);
        Assert.Equal(at.AddTicks(-7), back.History[0]);
    }

    [Fact]
    public void Uuid_Uses_Rfc_Byte_Order_Not_ToByteArray()
    {
        var g = Guid.Parse("0190f2a8-1b2c-7d3e-8f40-123456789abc");
        Assert.Equal("0190F2A81B2C7D3E8F40123456789ABC", Convert.ToHexString(AsterUuid.ToBytes(g)));
        Assert.NotEqual(Convert.ToHexString(g.ToByteArray()), Convert.ToHexString(AsterUuid.ToBytes(g)));
        Assert.Equal(g, AsterUuid.ToGuid(AsterUuid.ToBytes(g)));
        Assert.Throws<ContractValueException>(() => AsterUuid.ToGuid(new byte[15]));
    }

    [Fact]
    public void Ulid_Crockford_Round_Trip()
    {
        var u = Ulid.Parse("01arz3ndektsv4rrffq69g5fav");
        Assert.Equal("01ARZ3NDEKTSV4RRFFQ69G5FAV", u.ToString());
        Assert.Equal("01563E3AB5D3D6764C61EFB99302BD5B", Convert.ToHexString(u.ToBytes()));
        Assert.Equal(u, Ulid.FromBytes(u.ToBytes()));
        Assert.Equal(1469922850259L, u.Time.ToUnixTimeMilliseconds());
        Assert.Throws<FormatException>(() => Ulid.Parse("81ARZ3NDEKTSV4RRFFQ69G5FAV"));
        Assert.Throws<FormatException>(() => Ulid.Parse("01ARZ3NDEKTSV4RRFFQ69G5FAU"));
        var now = DateTimeOffset.FromUnixTimeMilliseconds(DateTimeOffset.UtcNow.ToUnixTimeMilliseconds());
        Assert.Equal(now, Ulid.NewUlid(now).Time);
    }

    [Fact]
    public void Decimal_Conversion_Is_Exact_Or_Refused()
    {
        Assert.Equal(new ForyDecimal(-1234567, 2), AsterDecimal.FromDecimal(-12345.67m));
        Assert.Equal(new ForyDecimal(15000, 4), AsterDecimal.FromDecimal(1.5000m));
        Assert.Equal(-12345.6700m, AsterDecimal.ToDecimal(new ForyDecimal(-123456700, 4)));
        Assert.Equal(decimal.MaxValue, AsterDecimal.ToDecimal(AsterDecimal.FromDecimal(decimal.MaxValue)));
        // 30 significant digits: more than System.Decimal's 96 bits, refused rather than rounded.
        Assert.Throws<OverflowException>(() => AsterDecimal.ToDecimal(AsterDecimal.Parse("12345678901234567890.1234567891")));
        Assert.Equal(12345678901234567890.123456789m, AsterDecimal.ToDecimal(AsterDecimal.Parse("12345678901234567890.1234567890")));
        // Trailing zeros beyond scale 28 drop exactly; a nonzero 29th place does not fit.
        Assert.Equal(1m, AsterDecimal.ToDecimal(new ForyDecimal(System.Numerics.BigInteger.Pow(10, 30), 30)));
        Assert.Throws<OverflowException>(() => AsterDecimal.ToDecimal(new ForyDecimal(1, 29)));
        Assert.Equal(1200m, AsterDecimal.ToDecimal(new ForyDecimal(12, -2)));
        Assert.Equal("-0.0001", AsterDecimal.Format(new ForyDecimal(-1, 4)));
        Assert.Equal("1200", AsterDecimal.Format(new ForyDecimal(12, -2)));
        Assert.Throws<ArgumentOutOfRangeException>(() => new WireDecimalAttribute(5, 6));
        Assert.Throws<ArgumentOutOfRangeException>(() => new WireDecimalAttribute(0, 0));
    }

    [Fact]
    public void Types_Without_A_Wire_Are_Refused_At_Contract_Time()
    {
        Assert.Throws<NotSupportedException>(() => ContractBuilder.TypeDefJson(typeof(BadDateTime)));
        Assert.Throws<NotSupportedException>(() => ContractBuilder.TypeDefJson(typeof(BadGuid)));
        Assert.Throws<NotSupportedException>(() => ContractBuilder.TypeDefJson(typeof(BadDecimal)));
    }

    [WireType("bad.v1/BadDateTime")] public sealed class BadDateTime { public DateTime At { get; set; } }
    [WireType("bad.v1/BadGuid")] public sealed class BadGuid { public Guid Id { get; set; } }
    [WireType("bad.v1/BadDecimal")] public sealed class BadDecimal { public decimal Amount { get; set; } }

    // ── helpers ─────────────────────────────────────────────────────────────

    private static object DecodeAs<T>(ForyCodec codec, byte[] bytes) => codec.Decode<T>(bytes)!;
    private static byte[] EncodeAs<T>(ForyCodec codec, object value) => codec.Encode((T)value);

    private static object Decode(ForyCodec codec, Type type, byte[] bytes)
    {
        try { return typeof(ContractTypeVectorTests).GetMethod(nameof(DecodeAs), BindingFlags.NonPublic | BindingFlags.Static)!.MakeGenericMethod(type).Invoke(null, [codec, bytes])!; }
        catch (TargetInvocationException e) when (e.InnerException is { } inner) { throw inner; }
    }

    private static byte[] Encode(ForyCodec codec, Type type, object value)
    {
        try { return (byte[])typeof(ContractTypeVectorTests).GetMethod(nameof(EncodeAs), BindingFlags.NonPublic | BindingFlags.Static)!.MakeGenericMethod(type).Invoke(null, [codec, value])!; }
        catch (TargetInvocationException e) when (e.InnerException is { } inner) { throw inner; }
    }

    /// <summary>The vector's JSON form of a decoded value (snake_case names; the contract types' string forms).</summary>
    private static JsonNode? ToJson(object? value, MemberInfo? member = null)
    {
        switch (value)
        {
            case null: return null;
            case string s: return s;
            case int i: return i;
            case DateTimeOffset t: return t.UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'", CultureInfo.InvariantCulture);
            case DateOnly d: return d.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture);
            case ForyDecimal dec: return AsterDecimal.Format(dec);
            case byte[] b:
                if (member?.GetCustomAttribute<WireUuidAttribute>() != null) return AsterUuid.Format(b);
                if (member?.GetCustomAttribute<WireUlidAttribute>() != null) return Ulid.FromBytes(b).ToString();
                return Convert.ToHexString(b).ToLowerInvariant();
            case IDictionary dict:
            {
                var o = new JsonObject();
                foreach (DictionaryEntry e in dict) o[ToJson(e.Key, member)!.GetValue<string>()] = ToJson(e.Value, member);
                return o;
            }
            case IEnumerable list:
            {
                var a = new JsonArray();
                foreach (var item in list) a.Add(ToJson(item, member));
                return a;
            }
            default:
            {
                var o = new JsonObject();
                foreach (var p in value.GetType().GetProperties(BindingFlags.Public | BindingFlags.Instance))
                    o[ContractBuilder.SnakeCase(p.Name)] = ToJson(p.GetValue(value), p);
                return o;
            }
        }
    }
}
