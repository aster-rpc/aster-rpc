using System;
using System.Linq;
using Aster.Crypto;

namespace Aster.Tests;

/// <summary>ABI 3.4 pure functions: ids, HPKE, attestation.</summary>
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

    [Fact]
    public void Attestation_Create_Text_Verify()
    {
        byte[] root = Ids.GenerateSecretKey();
        byte[] node = Ids.GenerateSecretKey();
        byte[] chain = Attestation.AttestRootNode(root, node);
        string text = Attestation.ToText(chain);
        Assert.StartsWith("aster.attestation.chain.v1:", text);
        Assert.Equal(chain, Attestation.FromText(text));

        var verified = Attestation.VerifyChain(chain, [Ids.PublicKey(root)], Ids.PublicKey(node));
        Assert.Equal(Ids.PublicKey(node), verified.Node);
        Assert.Equal(Ids.PublicKey(root), verified.Anchor);
        Assert.Empty(verified.Intermediates);
        Assert.Equal(AttestationRole.Node, verified.RoleOf(Ids.PublicKey(node)));
        Assert.Equal(AttestationRole.Anchor, verified.RoleOf(Ids.PublicKey(root)));
        Assert.Equal(AttestationRole.None, verified.RoleOf(new byte[32]));

        var other = Ids.PublicKey(Ids.GenerateSecretKey());
        Assert.Equal(Status.VerificationFailed,
            Assert.Throws<IrohException>(() => Attestation.VerifyChain(chain, [other], Ids.PublicKey(node))).Status);
        Assert.Equal(Status.VerificationFailed,
            Assert.Throws<IrohException>(() => Attestation.VerifyChain(chain, [Ids.PublicKey(root)], other)).Status);
    }

    [Fact]
    public void Attestation_Options_Are_Applied()
    {
        byte[] root = Ids.GenerateSecretKey();
        byte[] node = Ids.GenerateSecretKey();
        long now = DateTimeOffset.UtcNow.ToUnixTimeSeconds();
        byte[] expired = Attestation.AttestRootNode(root, node, new AttestOptions(1, now - 7200, now - 3600));
        Assert.Equal(Status.VerificationFailed,
            Assert.Throws<IrohException>(() => Attestation.VerifyChain(expired, [Ids.PublicKey(root)], Ids.PublicKey(node))).Status);
        byte[] valid = Attestation.AttestRootNode(root, node, new AttestOptions(1, now - 60, now + 3600));
        Assert.True(Attestation.VerifyChain(valid, [Ids.PublicKey(root)], Ids.PublicKey(node)).Depth >= 1);
    }
}
