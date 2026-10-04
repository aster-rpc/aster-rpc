using System;
using System.Collections.Generic;

namespace Aster.Crypto;

/// <summary>A key's role in a verified chain (<c>aster_attestation_role_t</c>).</summary>
public enum AttestationRole : uint
{
    None = 0,
    Node = 1,
    Anchor = 2,
    Intermediate = 3,
}

/// <summary>Validity window and epoch of a new attestation (Unix seconds; 0 = Rust default).</summary>
public sealed record AttestOptions(long Epoch = 0, long NotBefore = 0, long NotAfter = 0);

/// <summary>A chain that verified: the attested node, the trusted anchor, and any intermediates.</summary>
public sealed record VerifiedChain(byte[] Node, byte[] Anchor, IReadOnlyList<byte[]> Intermediates, int Depth)
{
    /// <summary>The role of <paramref name="id"/> in this chain (node wins over anchor if equal).</summary>
    public AttestationRole RoleOf(ReadOnlySpan<byte> id)
    {
        if (id.SequenceEqual(Node)) return AttestationRole.Node;
        if (id.SequenceEqual(Anchor)) return AttestationRole.Anchor;
        foreach (var i in Intermediates)
            if (id.SequenceEqual(i)) return AttestationRole.Intermediate;
        return AttestationRole.None;
    }
}

/// <summary>
/// Ownership attestation chains (C ABI 3.4, <c>aster_attest*</c>): create, convert to/from the
/// <c>aster.attestation.chain.v1:&lt;base64url&gt;</c> text form, and verify against trusted
/// anchors. Verification failures throw <see cref="IrohException"/> with
/// <see cref="Status.VerificationFailed"/>.
/// </summary>
public static unsafe class Attestation
{
    /// <summary>A one-link chain: <paramref name="rootSecret"/> attests the node of <paramref name="nodeSecret"/>.</summary>
    public static byte[] AttestRootNode(ReadOnlySpan<byte> rootSecret, ReadOnlySpan<byte> nodeSecret, AttestOptions? options = null)
    {
        Ids.Require32(rootSecret, nameof(rootSecret));
        Ids.Require32(nodeSecret, nameof(nodeSecret));
        Bytes output = default;
        AttestOptionsNative opts = default;
        if (options != null)
            opts = new AttestOptionsNative
            {
                struct_size = (uint)sizeof(AttestOptionsNative),
                epoch = options.Epoch,
                not_before = options.NotBefore,
                not_after = options.NotAfter,
            };
        fixed (byte* r = rootSecret)
        fixed (byte* n = nodeSecret)
            Native.Check(Native.aster_attest_root_node(r, n, options == null ? null : &opts, &output), "aster_attest_root_node");
        return NativeScope.TakeOwned(ref output);
    }

    public static string ToText(ReadOnlySpan<byte> chain)
    {
        using var scope = new NativeScope();
        Bytes output = default;
        Native.Check(Native.aster_attestation_chain_to_text(scope.Bytes(chain), &output), "aster_attestation_chain_to_text");
        return System.Text.Encoding.UTF8.GetString(NativeScope.TakeOwned(ref output));
    }

    public static byte[] FromText(string text)
    {
        using var scope = new NativeScope();
        Bytes output = default;
        Native.Check(Native.aster_attestation_chain_from_text(scope.Utf8(text), &output), "aster_attestation_chain_from_text");
        return NativeScope.TakeOwned(ref output);
    }

    /// <summary>
    /// Verifies, offline, that <paramref name="chain"/> attests <paramref name="expectedNode"/>
    /// (32-byte public key) and is anchored in one of <paramref name="trustedAnchors"/>.
    /// </summary>
    public static VerifiedChain VerifyChain(ReadOnlySpan<byte> chain, IReadOnlyList<byte[]> trustedAnchors, byte[] expectedNode)
    {
        var anchors = new byte[trustedAnchors.Count * Ids.KeySize];
        for (int i = 0; i < trustedAnchors.Count; i++)
        {
            Ids.Require32(trustedAnchors[i], nameof(trustedAnchors));
            trustedAnchors[i].CopyTo(anchors, i * Ids.KeySize);
        }
        Ids.Require32(expectedNode, nameof(expectedNode));
        using var scope = new NativeScope();
        var n = new VerifiedChainNative { struct_size = (uint)sizeof(VerifiedChainNative) };
        fixed (byte* a = anchors)
        fixed (byte* e = expectedNode)
            Native.Check(Native.aster_attestation_verify_chain(scope.Bytes(chain), anchors.Length == 0 ? null : a,
                (UIntPtr)trustedAnchors.Count, e, &n), "aster_attestation_verify_chain");
        try
        {
            var intermediates = new List<byte[]>();
            for (int i = 0; i < (int)n.intermediates_count; i++)
                intermediates.Add(new ReadOnlySpan<byte>((byte*)n.intermediates + i * Ids.KeySize, Ids.KeySize).ToArray());
            return new VerifiedChain(
                new ReadOnlySpan<byte>(n.node, Ids.KeySize).ToArray(),
                new ReadOnlySpan<byte>(n.anchor, Ids.KeySize).ToArray(),
                intermediates, (int)n.depth);
        }
        finally { Native.aster_verified_chain_release(&n); }
    }
}
