using System;
using System.Linq;
using Aster.Crypto;

namespace Aster.Tests;

/// <summary>ABI 3.4 pure functions: ids and HPKE.</summary>
public class CryptoTests
{
    [Fact]
    public void Ids_Parse_Format_Validate()
    {
        byte[] secret = Ids.GenerateSecretKey();
        string nodeId = Ids.NodeId(secret);
        Assert.Equal(64, nodeId.Length);
        Assert.Equal(nodeId, nodeId.ToLowerInvariant());
        byte[] pub = Ids.Parse(IdKind.NodeId, nodeId.ToUpperInvariant());
        Assert.Equal(Ids.PublicKey(secret), pub);
        Assert.Equal(nodeId, Ids.Format(IdKind.NodeId, pub));
        Assert.True(Ids.IsValid(IdKind.NodeId, pub));
        Assert.False(Ids.IsValid(IdKind.NodeId, new byte[31]));

        Assert.Throws<FormatException>(() => Ids.Parse(IdKind.Hash, "abc"));
        Assert.False(Ids.TryParse(IdKind.Hash, new string('z', 64), out _));
        Assert.True(Ids.TryParse(IdKind.Hash, new string('a', 64), out var hash));
        Assert.Equal(32, hash!.Length);
    }

    [Fact]
    public void Namespace_Id_Is_Derived_From_Secret()
    {
        byte[] secret = Ids.GenerateSecretKey();
        var id = Ids.NamespaceId(secret);
        Assert.Equal(32, id.Length);
        Assert.Equal(id, NamespaceCapability.Write(secret).NamespaceId());
        Assert.Equal(id, Ids.NamespaceId(secret));
    }

    [Fact]
    public void Hpke_Seal_Open_Round_Trip()
    {
        var kp = Hpke.GenerateKeyPair();
        Assert.Equal(kp.PublicKey, Hpke.PublicKeyFromPrivate(kp.PrivateKey));
        byte[] env = Hpke.Seal(kp.PublicKey, "aad"u8, "hello"u8);
        Assert.Equal("hello"u8.ToArray(), Hpke.Open(kp.PrivateKey, "aad"u8, env));
        var ex = Assert.Throws<IrohException>(() => Hpke.Open(kp.PrivateKey, "other"u8, env));
        Assert.Equal(Status.VerificationFailed, ex.Status);
    }

    [Fact]
    public void Hpke_Identity_Derivation_Is_Consistent()
    {
        byte[] secret = Ids.GenerateSecretKey();
        byte[] x25519Secret = Hpke.X25519SecretFromIdentity(secret);
        byte[] fromIdentity = Hpke.X25519PublicFromIdentity(Ids.PublicKey(secret));
        Assert.Equal(Hpke.PublicKeyFromPrivate(x25519Secret), fromIdentity);
        Assert.Equal(fromIdentity, Hpke.X25519PublicFromNodeId(Ids.NodeId(secret)));
    }
}
