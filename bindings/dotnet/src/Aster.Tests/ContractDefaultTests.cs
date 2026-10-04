using System;
using System.Collections.Generic;
using System.Text.Json;
using Aster.Rpc;

namespace Aster.Tests;

/// <summary>
/// <see cref="ContractDefaultAttribute"/> against <c>conformance/vectors/contract-defaults.json</c>
/// (produced by Rust's <c>#[aster(default = ...)]</c>): the same TypeDef bytes, type hash and
/// contract id, so .NET contracts with defaults get the Rust / Python / TS id.
/// </summary>
public class ContractDefaultTests
{
    /// <summary>The vector's <c>defaults.v1/Settings</c> message.</summary>
    [WireType("defaults.v1/Settings")]
    public class Settings
    {
        public string Name { get; set; } = "";
        [ContractDefault] public string State { get; set; } = "idle";
        [ContractDefault] public string Note { get; set; } = "";
        [ContractDefault] public bool Enabled { get; set; } = true;
        [ContractDefault] public int Retries { get; set; } = -3;
        [ContractDefault] public long SinceMs { get; set; } = 1_700_000_000_000;
        [ContractDefault] public double Ratio { get; set; } = 0.5;
        [ContractDefault] public string? Label { get; set; } = "x";
        [ContractDefault] public List<string> Tags { get; set; } = new();
        [ContractDefault] public Dictionary<string, string> Attrs { get; set; } = new();
        public byte[] Blob { get; set; } = [];
    }

    /// <summary>The same shape with every default dropped (all fields required).</summary>
    [WireType("defaults.v1/Settings")]
    public class SettingsRequired
    {
        public string Name { get; set; } = "";
        public string State { get; set; } = "idle";
        public string Note { get; set; } = "";
        public bool Enabled { get; set; } = true;
        public int Retries { get; set; } = -3;
        public long SinceMs { get; set; } = 1_700_000_000_000;
        public double Ratio { get; set; } = 0.5;
        public string? Label { get; set; } = "x";
        public List<string> Tags { get; set; } = new();
        public Dictionary<string, string> Attrs { get; set; } = new();
        public byte[] Blob { get; set; } = [];
    }

    private static ServiceDefinition Service<T>() where T : class, new()
        => ServiceDefinition.Create("Defaults", 1)
            .Unary<T, T>("put", (r, _) => ValueTask.FromResult(r))
            .Build();

    private static JsonElement Vector => Vectors.Load("contract-defaults.json").GetProperty("vector");

    [Fact]
    public void TypeDef_Matches_Rust_Vector()
    {
        string json = ContractBuilder.TypeDefJson(typeof(Settings));
        byte[] canonical = ContractIdentity.ComputeCanonicalBytes("TypeDef", json);
        Assert.Equal(Vector.Str("expected_canonical_type_def_hex"), Convert.ToHexString(canonical).ToLowerInvariant());
        Assert.Equal(Vector.Str("expected_type_hash"), ContractBuilder.TypeHash(typeof(Settings)));
    }

    [Fact]
    public void ContractId_Matches_Rust_Vector()
        => Assert.Equal(Vector.Str("expected_contract_id"), ContractBuilder.ComputeContractId(Service<Settings>()));

    [Fact]
    public void Fields_Match_Vector_Required_Optional_And_DefaultValue()
    {
        using var doc = JsonDocument.Parse(ContractBuilder.TypeDefJson(typeof(Settings)));
        var byName = new Dictionary<string, JsonElement>();
        foreach (var f in doc.RootElement.GetProperty("fields").EnumerateArray())
            byName[f.Str("name")] = f;
        foreach (var expected in Vector.GetProperty("type_def").GetProperty("fields_in_declaration_order").EnumerateArray())
        {
            var actual = byName[expected.Str("name")];
            Assert.Equal(expected.GetProperty("required").GetBoolean(), actual.GetProperty("required").GetBoolean());
            Assert.Equal(expected.GetProperty("optional").GetBoolean(), actual.GetProperty("optional").GetBoolean());
            Assert.Equal(expected.Str("default_value_hex"), actual.Str("default_value"));
            Assert.Equal(expected.Str("type_primitive"), actual.Str("type_primitive"));
        }
        Assert.Equal(11, byName.Count);
    }

    [Fact]
    public void Plain_Initializers_Stay_Required_And_Change_The_Contract_Id()
    {
        string withDefaults = ContractBuilder.ComputeContractId(Service<Settings>());
        string plain = ContractBuilder.ComputeContractId(Service<SettingsRequired>());
        Assert.NotEqual(withDefaults, plain);
        using var doc = JsonDocument.Parse(ContractBuilder.TypeDefJson(typeof(SettingsRequired)));
        foreach (var f in doc.RootElement.GetProperty("fields").EnumerateArray())
        {
            Assert.True(f.GetProperty("required").GetBoolean(), f.Str("name"));
            Assert.Equal("", f.Str("default_value"));
        }
    }

    [Fact]
    public void Changing_A_Default_Changes_The_Contract_Id()
        => Assert.NotEqual(ContractBuilder.ComputeContractId(Service<Settings>()),
            ContractBuilder.ComputeContractId(Service<ChangedDefault>()));

    [WireType("defaults.v1/Settings")]
    public class ChangedDefault
    {
        [ContractDefault] public int Retries { get; set; } = 4;
    }

    [WireType("t/Bad1")] public class NonEmptyList { [ContractDefault] public List<string> Tags { get; set; } = ["a"]; }
    [WireType("t/Bad2")] public class NullDefault { [ContractDefault] public string? Label { get; set; } }
    [WireType("t/Bad3")] public class ReservedFloat32 { [ContractDefault] public float Ratio { get; set; } = 1f; }
    [WireType("t/Bad4")] public class ReservedUInt8 { [ContractDefault] public byte B { get; set; } = 1; }
    [WireType("t/Bad5")] public class NaNDefault { [ContractDefault] public double D { get; set; } = double.NaN; }
    [WireType("t/Bad6")] public class SubnormalDefault { [ContractDefault] public double D { get; set; } = 1e-310; }
    [WireType("t/Bad7")] public class NoDefaultCtor { public NoDefaultCtor(int x) { X = x; } [ContractDefault] public int X { get; set; } }
    [WireType("t/Bad8")] public class RefDefault { [ContractDefault] public Settings S { get; set; } = new(); }

    [Theory]
    [InlineData(typeof(NonEmptyList))]
    [InlineData(typeof(NullDefault))]
    [InlineData(typeof(ReservedFloat32))]
    [InlineData(typeof(ReservedUInt8))]
    [InlineData(typeof(NaNDefault))]
    [InlineData(typeof(SubnormalDefault))]
    [InlineData(typeof(NoDefaultCtor))]
    [InlineData(typeof(RefDefault))]
    public void Unsupported_Defaults_Fail_Fast(Type type)
        => Assert.Throws<NotSupportedException>(() => ContractBuilder.TypeDefJson(type));

    [WireType("t/Edge")]
    public class Edge
    {
        [ContractDefault] public int Min { get; set; } = int.MinValue;
        [ContractDefault] public long Max { get; set; } = long.MaxValue;
        [ContractDefault] public double NegZero { get; set; } = -0.0;
        [ContractDefault] public int? Maybe { get; set; } = 7;
    }

    [Fact]
    public void Edge_Values_Are_Canonical()
    {
        using var doc = JsonDocument.Parse(ContractBuilder.TypeDefJson(typeof(Edge)));
        var f = new Dictionary<string, string>();
        foreach (var e in doc.RootElement.GetProperty("fields").EnumerateArray()) f[e.Str("name")] = e.Str("default_value");
        Assert.Equal("ffffffff0f", f["min"]);                 // zigzag(i32::MIN) = u32::MAX
        Assert.Equal("feffffffffffffffff01", f["max"]);       // zigzag(i64::MAX) = u64::MAX - 1
        Assert.Equal("0000000000000000", f["neg_zero"]);      // -0.0 normalised
        Assert.Equal("0e", f["maybe"]);                       // zigzag(7) = 14
    }
}
