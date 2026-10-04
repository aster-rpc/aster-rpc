using System;
using System.Text;

namespace Aster.Crypto;

/// <summary>Kinds of 32-byte identifiers (<c>aster_id_kind_t</c>).</summary>
public enum IdKind : uint
{
    NodeId = 0,
    PublicKey = 1,
    SecretKey = 2,
    Hash = 3,
    NamespaceId = 4,
    NamespaceSecret = 5,
    AuthorId = 6,
}

/// <summary>
/// Typed 32-byte ids (C ABI 3.4, <c>aster_id_*</c>): 64-hex parsing (either case; Ed25519 kinds
/// must be curve points), lowercase formatting, and key derivations. Pure functions, no runtime.
/// </summary>
public static unsafe class Ids
{
    public const int KeySize = 32;

    /// <summary>Parses exactly 64 hex chars; throws <see cref="FormatException"/> when invalid.</summary>
    public static byte[] Parse(IdKind kind, string hex)
    {
        var output = new byte[KeySize];
        using var scope = new NativeScope();
        int r;
        fixed (byte* o = output)
            r = Native.aster_id_parse((uint)kind, scope.Utf8(hex), o);
        if (r != 0) throw new FormatException($"invalid {kind}: {IrohException.FromStatus(r, "aster_id_parse").Message}");
        return output;
    }

    public static bool TryParse(IdKind kind, string hex, out byte[]? id)
    {
        try { id = Parse(kind, hex); return true; }
        catch (FormatException) { id = null; return false; }
    }

    /// <summary>Lowercase hex (does not validate).</summary>
    public static string Format(IdKind kind, ReadOnlySpan<byte> id)
    {
        Require32(id, nameof(id));
        byte* output = stackalloc byte[65];
        fixed (byte* p = id)
            Native.Check(Native.aster_id_format((uint)kind, p, output), "aster_id_format");
        return Encoding.ASCII.GetString(output, 64);
    }

    public static bool IsValid(IdKind kind, ReadOnlySpan<byte> id)
    {
        if (id.Length != KeySize) return false;
        fixed (byte* p = id)
            return Native.aster_id_validate((uint)kind, p) == 0;
    }

    /// <summary>A fresh random Ed25519 secret key.</summary>
    public static byte[] GenerateSecretKey()
    {
        var output = new byte[KeySize];
        UIntPtr len = KeySize;
        fixed (byte* o = output)
            Native.Check(Native.aster_secret_key_generate(o, (UIntPtr)KeySize, &len), "aster_secret_key_generate");
        return output;
    }

    /// <summary>The Ed25519 public key (= node id) of <paramref name="secretKey"/>.</summary>
    public static byte[] PublicKey(ReadOnlySpan<byte> secretKey) => Derive(secretKey, &Native.aster_secret_key_public_key, "aster_secret_key_public_key");

    /// <summary>The node id (64 hex) of <paramref name="secretKey"/>.</summary>
    public static string NodeId(ReadOnlySpan<byte> secretKey) => Format(IdKind.NodeId, PublicKey(secretKey));

    /// <summary>The namespace id of a namespace secret.</summary>
    public static byte[] NamespaceId(ReadOnlySpan<byte> namespaceSecret) => Derive(namespaceSecret, &Native.aster_namespace_secret_id, "aster_namespace_secret_id");

    internal static byte[] Derive(ReadOnlySpan<byte> input, delegate*<byte*, byte*, int> fn, string name)
    {
        Require32(input, nameof(input));
        var output = new byte[KeySize];
        fixed (byte* i = input)
        fixed (byte* o = output)
            Native.Check(fn(i, o), name);
        return output;
    }

    internal static void Require32(ReadOnlySpan<byte> key, string name)
    {
        if (key.Length != KeySize) throw new ArgumentException($"{name} must be {KeySize} bytes (got {key.Length})", name);
    }
}

/// <summary>
/// HPKE (Base mode, X25519 / HKDF-SHA256 / ChaCha20-Poly1305) through the C ABI. Envelopes are
/// Fory <c>aster.hpke.Envelope</c> bytes; a failed open throws <see cref="IrohException"/> with
/// <see cref="Status.VerificationFailed"/>.
/// </summary>
public static unsafe class Hpke
{
    public readonly record struct KeyPair(byte[] PrivateKey, byte[] PublicKey);

    public static KeyPair GenerateKeyPair()
    {
        var priv = new byte[Ids.KeySize];
        var pub = new byte[Ids.KeySize];
        fixed (byte* a = priv)
        fixed (byte* b = pub)
            Native.Check(Native.aster_hpke_generate_keypair(a, b), "aster_hpke_generate_keypair");
        return new KeyPair(priv, pub);
    }

    public static byte[] PublicKeyFromPrivate(ReadOnlySpan<byte> privateKey)
        => Ids.Derive(privateKey, &Native.aster_hpke_public_key_from_private, "aster_hpke_public_key_from_private");

    /// <summary>The X25519 secret for an Ed25519 identity secret (libsodium-compatible).</summary>
    public static byte[] X25519SecretFromIdentity(ReadOnlySpan<byte> identitySecret)
        => Ids.Derive(identitySecret, &Native.aster_hpke_x25519_secret_from_identity, "aster_hpke_x25519_secret_from_identity");

    /// <summary>The X25519 public key for an Ed25519 identity public key.</summary>
    public static byte[] X25519PublicFromIdentity(ReadOnlySpan<byte> identityPublic)
        => Ids.Derive(identityPublic, &Native.aster_hpke_x25519_public_from_identity, "aster_hpke_x25519_public_from_identity");

    /// <summary>The X25519 public key for a node id (64 hex).</summary>
    public static byte[] X25519PublicFromNodeId(string nodeIdHex)
    {
        var output = new byte[Ids.KeySize];
        using var scope = new NativeScope();
        fixed (byte* o = output)
            Native.Check(Native.aster_hpke_x25519_public_from_node_id(scope.Utf8(nodeIdHex), o), "aster_hpke_x25519_public_from_node_id");
        return output;
    }

    public static byte[] Seal(ReadOnlySpan<byte> recipientPublicKey, ReadOnlySpan<byte> aad, ReadOnlySpan<byte> plaintext)
    {
        Ids.Require32(recipientPublicKey, nameof(recipientPublicKey));
        using var scope = new NativeScope();
        Bytes output = default;
        fixed (byte* k = recipientPublicKey)
            Native.Check(Native.aster_hpke_seal(k, scope.Bytes(aad), scope.Bytes(plaintext), &output), "aster_hpke_seal");
        return NativeScope.TakeOwned(ref output);
    }

    public static byte[] Open(ReadOnlySpan<byte> recipientPrivateKey, ReadOnlySpan<byte> aad, ReadOnlySpan<byte> envelope)
    {
        Ids.Require32(recipientPrivateKey, nameof(recipientPrivateKey));
        using var scope = new NativeScope();
        Bytes output = default;
        fixed (byte* k = recipientPrivateKey)
            Native.Check(Native.aster_hpke_open(k, scope.Bytes(aad), scope.Bytes(envelope), &output), "aster_hpke_open");
        return NativeScope.TakeOwned(ref output);
    }
}
