using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text.Json;
using Aster.Crypto;
using Aster.Interceptors;

namespace Aster.Tests;

/// <summary>conformance/vectors/*.json: copied next to the test assembly, else found above it.</summary>
internal static class Vectors
{
    public static JsonElement Load(string name)
    {
        string local = Path.Combine(AppContext.BaseDirectory, "vectors", name);
        if (File.Exists(local))
        {
            using var copied = JsonDocument.Parse(File.ReadAllText(local));
            return copied.RootElement.Clone();
        }
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null && !File.Exists(Path.Combine(dir.FullName, "conformance", "vectors", name)))
            dir = dir.Parent;
        if (dir == null) throw new FileNotFoundException($"conformance/vectors/{name} not found above {AppContext.BaseDirectory}");
        using var doc = JsonDocument.Parse(File.ReadAllText(Path.Combine(dir.FullName, "conformance", "vectors", name)));
        return doc.RootElement.Clone();
    }

    public static byte[] Hex(this JsonElement e, string key)
        => e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.String ? Convert.FromHexString(v.GetString()!) : Array.Empty<byte>();

    public static string Str(this JsonElement e, string key) => e.GetProperty(key).GetString()!;
}

/// <summary>conformance/vectors/envelope.json through the C ABI AEv1 codec.</summary>
public class EnvelopeVectorTests
{
    private static List<KeyValuePair<string, string>> Pairs(JsonElement input, string keys, string values)
        => input.GetProperty(keys).EnumerateArray().Zip(input.GetProperty(values).EnumerateArray())
            .Select(p => new KeyValuePair<string, string>(p.First.GetString()!, p.Second.GetString()!)).ToList();

    public static IEnumerable<object[]> Valid()
        => Vectors.Load("envelope.json").GetProperty("vectors").EnumerateArray().Select(v => new object[] { v.Str("name") });

    public static IEnumerable<object[]> Invalid()
        => Vectors.Load("envelope.json").GetProperty("invalid").EnumerateArray().Select(v => new object[] { v.Str("name") });

    private static JsonElement Find(string section, string name)
        => Vectors.Load("envelope.json").GetProperty(section).EnumerateArray().First(v => v.Str("name") == name);

    [Theory]
    [MemberData(nameof(Valid))]
    public void Encodes_And_Decodes_Vector(string name)
    {
        var v = Find("vectors", name);
        var input = v.GetProperty("input");
        byte[] expected = v.Hex("hex");
        switch (v.Str("type"))
        {
            case "StreamHeader":
            {
                var h = new StreamHeader(input.Str("service"), input.Str("method"),
                    input.GetProperty("version").GetInt32(), input.GetProperty("callId").GetInt32(),
                    input.GetProperty("deadline").GetInt16(), (SerializationMode)input.GetProperty("serializationMode").GetSByte(),
                    Pairs(input, "metadataKeys", "metadataValues"), input.GetProperty("sessionId").GetInt32());
                Assert.Equal(expected, AsterEnvelope.Encode(h));
                var d = AsterEnvelope.DecodeStreamHeader(expected);
                Assert.Equal(h with { Metadata = d.Metadata }, d);
                Assert.Equal(h.Metadata, d.Metadata);
                break;
            }
            case "CallHeader":
            {
                var h = new CallHeader(input.Str("method"), input.GetProperty("callId").GetInt32(),
                    input.GetProperty("deadline").GetInt16(), Pairs(input, "metadataKeys", "metadataValues"));
                Assert.Equal(expected, AsterEnvelope.Encode(h));
                var d = AsterEnvelope.DecodeCallHeader(expected);
                Assert.Equal(h with { Metadata = d.Metadata }, d);
                Assert.Equal(h.Metadata, d.Metadata);
                break;
            }
            case "RpcStatus":
            {
                var s = new RpcStatus((StatusCode)input.GetProperty("code").GetInt32(), input.Str("message"),
                    Pairs(input, "detailKeys", "detailValues"));
                Assert.Equal(expected, AsterEnvelope.Encode(s));
                var d = AsterEnvelope.DecodeRpcStatus(expected);
                Assert.Equal(s with { Details = d.Details }, d);
                Assert.Equal(s.Details, d.Details);
                break;
            }
            default:
                throw new InvalidOperationException(v.Str("type"));
        }
    }

    [Theory]
    [MemberData(nameof(Invalid))]
    public void Rejects_Invalid_Vector(string name)
    {
        var v = Find("invalid", name);
        byte[] bytes = v.Hex("hex");
        var ex = Assert.Throws<IrohException>(() =>
        {
            switch (v.Str("type"))
            {
                case "StreamHeader": AsterEnvelope.DecodeStreamHeader(bytes); break;
                case "CallHeader": AsterEnvelope.DecodeCallHeader(bytes); break;
                default: AsterEnvelope.DecodeRpcStatus(bytes); break;
            }
        });
        Assert.Equal(Status.InvalidArgument, ex.Status);
    }

    [Fact]
    public void Trailer_Frame_Is_Framed_Aev1_RpcStatus()
    {
        byte[] frame = Rpc.ServerCall.TrailerFrame(new RpcStatus(StatusCode.Internal, "boom"));
        Assert.Equal((uint)(frame.Length - 4), BitConverter.ToUInt32(frame, 0));
        Assert.Equal(AsterFraming.FlagTrailer, frame[4]);
        Assert.Equal(AsterEnvelope.TagRpcStatus, frame[5]);
        var status = AsterEnvelope.DecodeRpcStatus(frame.AsSpan(5));
        Assert.Equal(StatusCode.Internal, status.Code);
        Assert.Equal("boom", status.Message);
    }
}

/// <summary>conformance/vectors/compression.json through <c>aster_zstd_*</c> and <see cref="AsterFraming"/>.</summary>
public class CompressionVectorTests
{
    private static byte[] Rule(JsonElement v)
    {
        int n = v.GetProperty("payload_length").GetInt32();
        var p = new byte[n];
        if (v.Str("payload_rule") == "byte[i] = i % 251")
            for (int i = 0; i < n; i++) p[i] = (byte)(i % 251);
        return p;
    }

    private static (byte Flags, byte[] Payload) Unframe(byte[] frame)
    {
        int body = (int)BitConverter.ToUInt32(frame, 0);
        Assert.Equal(frame.Length - 4, body);
        return (frame[4], frame.AsSpan(5).ToArray());
    }

    [Fact]
    public void Decode_Vectors_Decompress_To_Payload()
    {
        foreach (var v in Vectors.Load("compression.json").GetProperty("decode_vectors").EnumerateArray())
        {
            var (flags, payload) = Unframe(v.Hex("frame_hex"));
            Assert.Equal(v.GetProperty("flags").GetInt32(), flags);
            Assert.Equal(Rule(v), AsterFraming.PayloadOf(payload, flags));
        }
    }

    [Fact]
    public void Encode_Vectors_Match()
    {
        foreach (var v in Vectors.Load("compression.json").GetProperty("encode_vectors").EnumerateArray())
        {
            byte[] frame = AsterFraming.EncodePayloadFrame(Rule(v), (byte)v.GetProperty("input_flags").GetInt32());
            Assert.Equal(v.Hex("expected_frame_hex"), frame);
        }
    }

    [Fact]
    public void Reject_Vectors_Throw()
    {
        foreach (var v in Vectors.Load("compression.json").GetProperty("reject_vectors").EnumerateArray())
        {
            var (flags, payload) = Unframe(v.Hex("frame_hex"));
            Assert.Throws<IrohException>(() => AsterFraming.PayloadOf(payload, flags));
        }
    }

    [Fact]
    public void Large_Compressible_Payload_Round_Trips_Compressed()
    {
        var payload = Enumerable.Repeat((byte)7, 20_000).ToArray();
        byte[] frame = AsterFraming.EncodePayloadFrame(payload, AsterFraming.FlagEndStream);
        var (flags, body) = Unframe(frame);
        Assert.Equal(AsterFraming.FlagCompressed | AsterFraming.FlagEndStream, flags);
        Assert.True(body.Length < payload.Length);
        Assert.Equal(payload, AsterFraming.PayloadOf(body, flags));
    }

    [Fact]
    public void Threshold_Payload_Is_Not_Compressed()
        => Assert.Null(Zstd.MaybeCompress(new byte[Zstd.CompressionThreshold]));
}

/// <summary>conformance/vectors/grants.json through the C ABI 3.4 grant functions.</summary>
public class GrantVectorTests
{
    private static readonly JsonElement G = Vectors.Load("grants.json");

    private static GrantContext Ctx(JsonElement v)
    {
        var c = v.GetProperty("context");
        return new GrantContext(c.Str("app"), c.Str("granter"), c.Str("recipient"), c.Hex("resource_hex"), c.Str("path"), c.Str("role"));
    }

    [Fact]
    public void Aad()
    {
        foreach (var v in G.GetProperty("aad").EnumerateArray())
            Assert.Equal(v.Hex("aad_hex"), Grants.Aad(Ctx(v)));
    }

    [Fact]
    public void GrantKey()
    {
        foreach (var v in G.GetProperty("grant_key").EnumerateArray())
            Assert.Equal(v.Str("key"), Grants.GrantKey(v.Str("prefix"), v.Str("recipient")));
    }

    [Fact]
    public void RoleRule()
    {
        foreach (var v in G.GetProperty("role_rule").EnumerateArray())
        {
            bool canWrite = v.GetProperty("can_write").GetBoolean();
            bool ok = v.GetProperty("ok").GetBoolean();
            Assert.Equal(ok, Grants.RoleMatches(v.Str("role"), canWrite));
            if (!ok)
                Assert.Equal(Status.VerificationFailed, Assert.Throws<IrohException>(() => Grants.CheckRole(v.Str("role"), canWrite)).Status);
        }
    }

    [Fact]
    public void Open()
    {
        foreach (var v in G.GetProperty("open").EnumerateArray())
        {
            byte[] key = v.Hex("recipient_key_hex");
            byte[] opened = v.Str("scheme") == "identity"
                ? Grants.Open(key, Ctx(v), v.Hex("envelope_hex"))
                : Grants.OpenWithKey(key, Ctx(v), v.Hex("envelope_hex"));
            Assert.Equal(v.Hex("plaintext_hex"), opened);
            if (v.TryGetProperty("recipient_public_hex", out _))
                Assert.Equal(v.Hex("recipient_public_hex"), Hpke.PublicKeyFromPrivate(key));
        }
    }

    [Fact]
    public void Open_Invalid_Fails_Verification()
    {
        foreach (var v in G.GetProperty("open_invalid").EnumerateArray())
        {
            var ex = Assert.Throws<IrohException>(() => Grants.Open(v.Hex("recipient_key_hex"), Ctx(v), v.Hex("envelope_hex")));
            Assert.True(ex.Status is Status.VerificationFailed or Status.InvalidArgument, $"{v.Str("name")}: {ex.Status}");
        }
    }

    [Fact]
    public void NamespaceOpen()
    {
        foreach (var v in G.GetProperty("namespace_open").EnumerateArray())
        {
            var cap = Grants.OpenNamespace(v.Hex("recipient_secret_hex"), Ctx(v), v.Hex("envelope_hex"));
            Assert.Equal(v.GetProperty("can_write").GetBoolean(), cap.CanWrite);
            Assert.Equal(v.Hex("capability_hex"), cap.Encode());
            Assert.Equal(cap, NamespaceCapability.Decode(v.Hex("capability_hex")));
        }
        foreach (var v in G.GetProperty("namespace_open_invalid").EnumerateArray())
            Assert.Equal(Status.VerificationFailed,
                Assert.Throws<IrohException>(() => Grants.OpenNamespace(v.Hex("recipient_secret_hex"), Ctx(v), v.Hex("envelope_hex"))).Status);
    }

    [Fact]
    public void Reconcile()
    {
        foreach (var v in G.GetProperty("reconcile").EnumerateArray())
        {
            string name = v.Str("name");
            byte[]? record = v.GetProperty("record_hex").ValueKind == JsonValueKind.Null ? null : v.Hex("record_hex");
            var outcome = v.GetProperty("outcome");
            string kind = outcome.Str("kind");
            byte[] secret = v.Hex("recipient_secret_hex");
            if (v.Str("payload") == "namespace")
            {
                var o = Grants.ReconcileNamespace(secret, Ctx(v), record);
                Assert.Equal(kind, o.Kind.ToString().ToLowerInvariant());
                if (o is GrantOutcome<NamespaceCapability>.Active a)
                    Assert.Equal(outcome.Hex("capability_hex"), a.Value.Encode());
                if (o is GrantOutcome<NamespaceCapability>.Revoked r)
                    Assert.Equal((outcome.Str("reason"), outcome.Str("role")), (r.Reason, r.Role));
            }
            else
            {
                var o = Grants.Reconcile(secret, Ctx(v), record);
                Assert.True(kind == o.Kind.ToString().ToLowerInvariant(), $"{name}: expected {kind}, got {o.Kind}");
                switch (o)
                {
                    case GrantOutcome<byte[]>.Active a:
                        Assert.Equal(outcome.Hex("plaintext_hex"), a.Value);
                        break;
                    case GrantOutcome<byte[]>.Revoked r:
                        Assert.Equal((outcome.Str("reason"), outcome.Str("role")), (r.Reason, r.Role));
                        break;
                    case GrantOutcome<byte[]>.Unopenable u when outcome.TryGetProperty("error_contains", out var contains):
                        Assert.Contains(contains.GetString()!, u.Reason);
                        break;
                }
            }
            if (record != null && kind is "active" or "revoked")
                Assert.Equal(record, GrantRecord.Decode(record).Encode());
        }
    }

    [Fact]
    public void Seal_Then_Open_Round_Trips()
    {
        byte[] recipientSecret = Ids.GenerateSecretKey();
        byte[] granterSecret = Ids.GenerateSecretKey();
        var ctx = new GrantContext("app/test/v1", Ids.NodeId(granterSecret), Ids.NodeId(recipientSecret), [1, 2, 3], "/app/v1/grants/x", "write");
        byte[] env = Grants.Seal(ctx, "secret"u8);
        Assert.Equal("secret"u8.ToArray(), Grants.Open(recipientSecret, ctx, env));
        Assert.Equal(Status.VerificationFailed,
            Assert.Throws<IrohException>(() => Grants.Open(recipientSecret, ctx with { Role = "read" }, env)).Status);

        var record = GrantRecord.Active("write", 1, env);
        var outcome = Grants.Reconcile(recipientSecret, ctx, record.Encode());
        Assert.Equal("secret"u8.ToArray(), Assert.IsType<GrantOutcome<byte[]>.Active>(outcome).Value);
        Assert.IsType<GrantOutcome<byte[]>.Absent>(Grants.Reconcile(recipientSecret, ctx, null));

        var ns = NamespaceCapability.Write(Ids.GenerateSecretKey());
        var nsEnv = Grants.SealNamespace(ctx, ns);
        Assert.Equal(ns, Grants.OpenNamespace(recipientSecret, ctx, nsEnv));
    }
}
