using System.Reflection;
using System.Text.Json;
using System.Text.Json.Nodes;
using Apache.Fory;
using Aster.Codec;
using Aster.Rpc;

namespace Aster.Tests;

// conformance/vectors/contract-types.json `field_names` (ffi_spec/Aster-FieldNaming.md): the Orders
// contract as Rust defines it (order_id) and as Java defines it (orderId), both through [WireName].

[ForyStruct, WireType("names.v1/Order")]
public sealed class FnRsOrder
{
    [WireName("order_id")] public string OrderId { get; set; } = "";
    [WireName("line_items")] public List<FnRsLineItem> LineItems { get; set; } = new();
    [WireName("total_cents")] public int TotalCents { get; set; }
    [WireName("note")] public string? Note { get; set; }
}

[ForyStruct, WireType("names.v1/LineItem")]
public sealed class FnRsLineItem
{
    [WireName("sku_code")] public string SkuCode { get; set; } = "";
    [WireName("unit_count")] public int UnitCount { get; set; }
}

[ForyStruct, WireType("names.v1/Order")]
public sealed class FnJvOrder
{
    [WireName("orderId")] public string OrderId { get; set; } = "";
    [WireName("lineItems")] public List<FnJvLineItem> LineItems { get; set; } = new();
    [WireName("totalCents")] public int TotalCents { get; set; }
    [WireName("note")] public string? Note { get; set; }
}

[ForyStruct, WireType("names.v1/LineItem")]
public sealed class FnJvLineItem
{
    [WireName("skuCode")] public string SkuCode { get; set; } = "";
    [WireName("unitCount")] public int UnitCount { get; set; }
}

// Refused when the contract is built.
// Not [ForyStruct]: Fory C#'s generator refuses some of these itself.
[WireType("names.v1/Bad")]
public sealed class FnAcronym { public string UserID { get; set; } = ""; }

// Not [ForyStruct]: Fory C#'s generator refuses some of these itself.
[WireType("names.v1/Bad")]
public sealed class FnMismatch { [WireName("writtenBy")] public string Author { get; set; } = ""; }

// Not [ForyStruct]: Fory C#'s generator refuses some of these itself.
[WireType("names.v1/Bad")]
public sealed class FnCollide
{
    public string UserId { get; set; } = "";
    public string User_id { get; set; } = "";
}

[ForyStruct, WireType("names.v1/Free")]
public sealed class FnVerbatim { public string WrittenBy { get; set; } = ""; }

public class FieldNameVectorTests
{
    private static JsonElement V => Vectors.Load("contract-types.json").GetProperty("vector").GetProperty("field_names");

    [Fact]
    public void Fory_CSharp_Snake_Case_And_Plainness_Match_The_Vector()
    {
        // The Fory C# runtime's own conversion (internal), pinned so an upgrade that changes it fails here.
        var foryUtils = typeof(ForyStructAttribute).Assembly.GetType("Apache.Fory.TypeMetaUtils", throwOnError: true)!
            .GetMethod("LowerCamelToLowerUnderscore", BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Static)!;
        foreach (var row in V.GetProperty("snake_case").EnumerateObject())
        {
            string name = row.Name;
            Assert.Equal(row.Value.Str("csharp"), (string)foryUtils.Invoke(null, [name])!);
            Assert.Equal(row.Value.Str("csharp"), FieldNames.ForySnakeCase(name));
            Assert.Equal(row.Value.Str("rust"), FieldNames.ReferenceSnakeCase(name));
            bool plain = row.Value.GetProperty("plain").GetBoolean();
            Assert.True(plain == (FieldNames.NotPlain(name) == null), name);
            if (!plain) Assert.Null(FieldNames.NotPlain(FieldNames.PlainSuggestion(name)));
        }
    }

    private static List<(string, string)> Pairs(JsonElement c)
        => c.GetProperty("fields").EnumerateArray().Select(f => (f.Str("member_snake"), f.Str("name"))).ToList();

    [Fact]
    public void Refused_And_Accepted_Cases_Match_The_Vector()
    {
        foreach (var c in V.GetProperty("refused").EnumerateArray())
        {
            var e = Assert.Throws<ArgumentException>(() => FieldNames.Check(Pairs(c)));
            Assert.Contains(c.Str("error"), e.Message);
        }
        foreach (var c in V.GetProperty("accepted").EnumerateArray()) FieldNames.Check(Pairs(c));
    }

    [Fact]
    public void CSharp_Types_Are_Refused_When_The_Contract_Is_Built()
    {
        Assert.Contains("not a plain field name", Assert.Throws<ArgumentException>(() => ContractBuilder.TypeHash(typeof(FnAcronym))).Message);
        Assert.Contains("does not match", Assert.Throws<ArgumentException>(() => ContractBuilder.TypeHash(typeof(FnMismatch))).Message);
        Assert.Contains("collide", Assert.Throws<ArgumentException>(() => ContractBuilder.TypeHash(typeof(FnCollide))).Message);
        // Without an override a C# contract's names are the member names, verbatim.
        Assert.Contains("\"name\":\"WrittenBy\"", ContractBuilder.TypeDefJson(typeof(FnVerbatim)));
    }

    public static IEnumerable<object[]> Definitions() => [["rust_defined"], ["java_defined"]];

    [Theory]
    [MemberData(nameof(Definitions))]
    public void Orders_Contract_Hashes_Ids_And_Payload_Match(string def)
    {
        var d = V.GetProperty(def);
        bool rust = def == "rust_defined";
        Type order = rust ? typeof(FnRsOrder) : typeof(FnJvOrder);
        Type item = rust ? typeof(FnRsLineItem) : typeof(FnJvLineItem);
        foreach (var (fqn, type) in new[] { ("names.v1.Order", order), ("names.v1.LineItem", item) })
        {
            var t = d.GetProperty("types").GetProperty(fqn);
            byte[] canonical = ContractIdentity.ComputeCanonicalBytes("TypeDef", ContractBuilder.TypeDefJson(type));
            Assert.Equal(t.Str("expected_canonical_type_def_hex"), Convert.ToHexString(canonical).ToLowerInvariant());
            Assert.Equal(t.Str("expected_type_hash"), ContractBuilder.TypeHash(type));
        }
        var svc = rust
            ? ServiceDefinition.Create("Orders", 1).Unary<FnRsOrder, FnRsOrder>("place", (r, _) => ValueTask.FromResult(r)).Build()
            : ServiceDefinition.Create("Orders", 1).Unary<FnJvOrder, FnJvOrder>("place", (r, _) => ValueTask.FromResult(r)).Build();
        Assert.Equal(d.Str("expected_contract_id"), ContractBuilder.ComputeContractId(svc));

        var p = d.GetProperty("payload");
        using var codec = new ForyCodec();
        object decoded, back;
        if (rust)
        {
            codec.Register<FnRsLineItem>("names.v1/LineItem").Register<FnRsOrder>("names.v1/Order");
            var o = codec.Decode<FnRsOrder>(p.Hex("hex"))!;
            decoded = o;
            back = codec.Decode<FnRsOrder>(codec.Encode(o))!;
        }
        else
        {
            codec.Register<FnJvLineItem>("names.v1/LineItem").Register<FnJvOrder>("names.v1/Order");
            var o = codec.Decode<FnJvOrder>(p.Hex("hex"))!;
            decoded = o;
            back = codec.Decode<FnJvOrder>(codec.Encode(o))!;
        }
        var expected = JsonNode.Parse(p.GetProperty("value").GetRawText());
        var got = ContractTypeVectorTests.ToJson(decoded);
        Assert.True(JsonNode.DeepEquals(expected, got), $"{def}: {got?.ToJsonString()}");
        // Fory C# may write other type-meta flags than Rust; both decode everywhere.
        Assert.True(JsonNode.DeepEquals(expected, ContractTypeVectorTests.ToJson(back)), $"{def}: re-encoded value differs");
    }
}
