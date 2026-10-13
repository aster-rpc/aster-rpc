using System;
using System.Collections.Generic;
using System.Numerics;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Aster.Tests;

/// <summary>
/// Plays an Enrollment root in tests (the .NET mirror of Go's <c>internal/testroot</c>): it signs
/// the credentials and delegations <c>aster enroll node</c> and <c>aster enroll delegate</c>
/// produce (compact Ed25519 JWT delegation chains) and builds the revocation record
/// <c>aster enroll revoke</c> publishes. Not part of the binding's API: root operations belong to
/// the <c>aster</c> CLI. Rust verifying links signed here doubles as a cross-implementation check.
/// </summary>
internal sealed class TestRoot
{
    private readonly byte[] _seed;
    private readonly string _audience;

    public TestRoot(byte[] seed, string audience)
    {
        _seed = seed;
        _audience = audience;
        PublicKey = Convert.ToHexString(Ed25519.PublicKey(seed)).ToLowerInvariant();
    }

    /// <summary>The root public key, 64 hex (<c>[trust] root_pubkey</c>).</summary>
    public string PublicKey { get; }

    /// <summary>Claims carrying roles in <c>aster.role</c> (what <c>--attributes roles.json</c> writes); empty = no attribute.</summary>
    public static Dictionary<string, object?> RoleClaims(string roles, string[]? scopes = null, string[]? tags = null)
    {
        var attributes = new Dictionary<string, object?>();
        if (roles.Length > 0)
            attributes["aster.role"] = new Dictionary<string, object?> { ["type"] = "string", ["value"] = roles };
        return new() { ["scopes"] = scopes ?? [], ["tags"] = tags ?? [], ["attributes"] = attributes };
    }

    // Rust borrows header strings: no \u escapes (e.g. "+" in the typ).
    private static readonly JsonSerializerOptions Relaxed = new() { Encoder = System.Text.Encodings.Web.JavaScriptEncoder.UnsafeRelaxedJsonEscaping };

    private static string B64(byte[] b) => Convert.ToBase64String(b).TrimEnd('=').Replace('+', '-').Replace('/', '_');

    private string Link(string typ, string subject, TimeSpan lifetime, string extension, object body)
    {
        byte[] sub = Convert.FromHexString(subject);
        if (sub.Length != 32) throw new ArgumentException($"subject {subject}: want a 64-hex node id");
        long now = DateTimeOffset.UtcNow.ToUnixTimeSeconds();
        var payload = new Dictionary<string, object?>
        {
            ["iss"] = PublicKey,
            ["sub"] = subject,
            ["cnf"] = new Dictionary<string, object?> { ["jwk"] = new Dictionary<string, string> { ["kty"] = "OKP", ["crv"] = "Ed25519", ["x"] = B64(sub) } },
            ["aud"] = _audience,
            ["jti"] = B64(RandomNumberGenerator.GetBytes(16)),
            ["iat"] = now,
            ["exp"] = now + (long)lifetime.TotalSeconds,
            [extension] = body,
        };
        string header = B64(JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, string> { ["alg"] = "EdDSA", ["typ"] = typ }, Relaxed));
        string input = header + "." + B64(JsonSerializer.SerializeToUtf8Bytes(payload, Relaxed));
        return input + "." + B64(Ed25519.Sign(_seed, Encoding.ASCII.GetBytes(input)));
    }

    /// <summary>A one-link, root-issued, ephemeral credential chain for <paramref name="subject"/> (<c>aster enroll node --out</c>).</summary>
    public string Credential(string subject, Dictionary<string, object?> claims, TimeSpan lifetime)
        => "aster-chain-v1~" + Link("aster-enrollment-v1+jwt", subject, lifetime, "aster_enrollment_v1", new Dictionary<string, object?>
        {
            ["scopes"] = claims["scopes"],
            ["tags"] = claims["tags"],
            ["attributes"] = claims["attributes"],
            ["mandatory"] = Array.Empty<string>(),
            ["registration"] = "ephemeral",
        });

    /// <summary>Lets <paramref name="subject"/> (an online issuer) enroll ephemeral nodes carrying <paramref name="attributeKeys"/> (<c>aster enroll delegate --out</c>).</summary>
    public string Delegation(string subject, string[] scopes, string[] attributeKeys, TimeSpan lifetime)
        => "aster-chain-v1~" + Link("aster-delegation-v1+jwt", subject, lifetime, "aster_delegation_v1", new Dictionary<string, object?>
        {
            ["may_issue"] = new[] { "enrollment" },
            ["audiences"] = new[] { _audience },
            ["scopes"] = scopes,
            ["tags"] = Array.Empty<string>(),
            ["attribute_keys"] = attributeKeys,
            ["max_child_lifetime"] = 3600,
            ["max_depth"] = 0,
            ["subjects"] = null,
            ["operations"] = Array.Empty<string>(),
            ["mandatory"] = Array.Empty<string>(),
            ["may_register"] = false,
        });

    /// <summary>The fleet-namespace key of <paramref name="subject"/>'s registration record.</summary>
    public static string RegistrationKey(string subject) => "aster/enrollment/v1/nodes/" + subject;

    /// <summary>The root <c>NodeRegistrationV1</c> record revoking <paramref name="subject"/> (<c>aster enroll revoke --fleet-only</c>).</summary>
    public byte[] Revocation(string subject, string reason)
        => JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["version"] = 1,
            ["issuer"] = PublicKey,
            ["subject"] = subject,
            ["audience"] = _audience,
            ["revision"] = 1,
            ["issued_at"] = DateTimeOffset.UtcNow.ToUnixTimeSeconds() - 60,
            ["status"] = "revoked",
            ["claims"] = new Dictionary<string, object?> { ["scopes"] = Array.Empty<string>(), ["tags"] = Array.Empty<string>(), ["attributes"] = new Dictionary<string, object?>() },
            ["mandatory"] = Array.Empty<string>(),
            ["reauthorizes_revision"] = null,
            ["reason"] = reason,
        });
}

/// <summary>Ed25519 signing (RFC 8032 §5.1, reference arithmetic); test-only, not constant time.</summary>
internal static class Ed25519
{
    private static readonly BigInteger P = BigInteger.Pow(2, 255) - 19;
    private static readonly BigInteger Q = BigInteger.Pow(2, 252) + BigInteger.Parse("27742317777372353535851937790883648493");
    private static readonly BigInteger D = Mod(-121665 * Inv(121666));
    private static readonly BigInteger SqrtM1 = BigInteger.ModPow(2, (P - 1) / 4, P);
    private static readonly (BigInteger X, BigInteger Y, BigInteger Z, BigInteger T) G = BasePoint();

    private static BigInteger Mod(BigInteger a, BigInteger? m = null)
    {
        var n = m ?? P;
        var r = a % n;
        return r.Sign < 0 ? r + n : r;
    }

    private static BigInteger Inv(BigInteger x) => BigInteger.ModPow(x, P - 2, P);

    private static (BigInteger, BigInteger, BigInteger, BigInteger) BasePoint()
    {
        var y = Mod(4 * Inv(5));
        var x2 = Mod((y * y - 1) * Inv(D * y * y + 1));
        var x = BigInteger.ModPow(x2, (P + 3) / 8, P);
        if (Mod(x * x - x2) != 0) x = Mod(x * SqrtM1);
        if (!x.IsEven) x = P - x;
        return (x, y, 1, Mod(x * y));
    }

    private static (BigInteger, BigInteger, BigInteger, BigInteger) Add(
        (BigInteger X, BigInteger Y, BigInteger Z, BigInteger T) p, (BigInteger X, BigInteger Y, BigInteger Z, BigInteger T) q)
    {
        var a = Mod((p.Y - p.X) * (q.Y - q.X));
        var b = Mod((p.Y + p.X) * (q.Y + q.X));
        var c = Mod(2 * p.T * q.T * D);
        var d = Mod(2 * p.Z * q.Z);
        BigInteger e = b - a, f = d - c, g = d + c, h = b + a;
        return (Mod(e * f), Mod(g * h), Mod(f * g), Mod(e * h));
    }

    private static (BigInteger, BigInteger, BigInteger, BigInteger) Mul(BigInteger s, (BigInteger, BigInteger, BigInteger, BigInteger) p)
    {
        (BigInteger, BigInteger, BigInteger, BigInteger) q = (0, 1, 1, 0);
        while (s > 0)
        {
            if (!s.IsEven) q = Add(q, p);
            p = Add(p, p);
            s >>= 1;
        }
        return q;
    }

    private static byte[] Compress((BigInteger X, BigInteger Y, BigInteger Z, BigInteger T) p)
    {
        var zi = Inv(p.Z);
        var x = Mod(p.X * zi);
        var y = Mod(p.Y * zi);
        return To32(y | ((x & 1) << 255));
    }

    private static byte[] To32(BigInteger v)
    {
        var b = new byte[32];
        v.ToByteArray(isUnsigned: true, isBigEndian: false).CopyTo(b, 0);
        return b;
    }

    private static BigInteger Le(ReadOnlySpan<byte> b) => new(b, isUnsigned: true, isBigEndian: false);

    private static (BigInteger A, byte[] Prefix) Expand(byte[] seed)
    {
        byte[] h = SHA512.HashData(seed);
        var a = Le(h.AsSpan(0, 32)) & ((BigInteger.One << 254) - 8);
        a |= BigInteger.One << 254;
        return (a, h[32..]);
    }

    public static byte[] PublicKey(byte[] seed) => Compress(Mul(Expand(seed).A, G));

    public static byte[] Sign(byte[] seed, byte[] message)
    {
        var (a, prefix) = Expand(seed);
        byte[] pub = Compress(Mul(a, G));
        var r = Mod(Le(SHA512.HashData([.. prefix, .. message])), Q);
        byte[] rs = Compress(Mul(r, G));
        var h = Mod(Le(SHA512.HashData([.. rs, .. pub, .. message])), Q);
        var s = Mod(r + h * a, Q);
        return [.. rs, .. To32(s)];
    }
}
