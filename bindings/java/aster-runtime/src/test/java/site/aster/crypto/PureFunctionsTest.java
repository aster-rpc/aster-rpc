package site.aster.crypto;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.foreign.MemoryLayout;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.List;
import java.util.Optional;
import org.junit.jupiter.api.Test;
import site.aster.attestation.AttestOptions;
import site.aster.attestation.Attestation;
import site.aster.attestation.AttestationRole;
import site.aster.attestation.VerifiedChain;
import site.aster.exception.VerificationFailedException;
import site.aster.id.IdKind;
import site.aster.id.Ids;

/** Ids, HPKE and attestation through the C ABI 3.4 pure functions. */
final class PureFunctionsTest {

  private static byte[] fill(int b) {
    byte[] k = new byte[32];
    Arrays.fill(k, (byte) b);
    return k;
  }

  private static byte[] utf8(String s) {
    return s.getBytes(StandardCharsets.UTF_8);
  }

  @Test
  void layoutsMatchHeader() {
    assertEquals(32, Attestation.ATTEST_OPTIONS.byteSize());
    assertEquals(96, Attestation.VERIFIED_CHAIN.byteSize());
    assertEquals(
        72,
        Attestation.VERIFIED_CHAIN.byteOffset(
            MemoryLayout.PathElement.groupElement("intermediates")));
    assertEquals(
        88,
        Attestation.VERIFIED_CHAIN.byteOffset(MemoryLayout.PathElement.groupElement("storage")));
  }

  @Test
  void idsParseFormatValidate() {
    byte[] pk = Ids.publicKey(fill(7));
    String hex = Ids.format(IdKind.NODE_ID, pk);
    assertEquals(HexFormat.of().formatHex(pk), hex);
    assertArrayEquals(pk, Ids.parse(IdKind.NODE_ID, hex.toUpperCase()));
    assertEquals(hex, Ids.nodeId(fill(7)));
    assertTrue(Ids.isValid(IdKind.PUBLIC_KEY, pk));
    byte[] offCurve = new byte[32];
    offCurve[0] = 2;
    assertFalse(Ids.isValid(IdKind.NODE_ID, offCurve));
    assertTrue(Ids.isValid(IdKind.HASH, offCurve));
    assertThrows(IllegalArgumentException.class, () -> Ids.validate(IdKind.AUTHOR_ID, offCurve));
    assertFalse(Ids.isValid(IdKind.HASH, "abc"));
    assertFalse(Ids.isValid(IdKind.HASH, " " + hex.substring(1)));
    assertThrows(IllegalArgumentException.class, () -> Ids.format(IdKind.HASH, new byte[31]));
    assertEquals(32, Ids.namespaceId(fill(3)).length);
    assertFalse(Arrays.equals(Ids.namespaceId(fill(3)), Ids.namespaceId(fill(4))));
  }

  @Test
  void hpkeRoundTripAndIdentityKeys() {
    Hpke.KeyPair kp = Hpke.generateKeyPair();
    assertArrayEquals(kp.publicKey(), Hpke.publicKeyFromPrivate(kp.privateKey()));
    byte[] env = Hpke.seal(kp.publicKey(), utf8("aad"), utf8("hello"));
    assertArrayEquals(utf8("hello"), Hpke.open(kp.privateKey(), utf8("aad"), env));
    assertThrows(
        VerificationFailedException.class, () -> Hpke.open(kp.privateKey(), utf8("x"), env));
    assertArrayEquals(
        new byte[0],
        Hpke.open(
            kp.privateKey(), new byte[0], Hpke.seal(kp.publicKey(), new byte[0], new byte[0])));

    byte[] idSecret = fill(9);
    byte[] idPublic = Ids.publicKey(idSecret);
    byte[] xs = Hpke.x25519SecretFromIdentity(idSecret);
    byte[] xp = Hpke.x25519PublicFromIdentity(idPublic);
    assertArrayEquals(xp, Hpke.publicKeyFromPrivate(xs));
    assertArrayEquals(xp, Hpke.x25519PublicFromNodeId(Ids.nodeId(idSecret)));
    assertThrows(IllegalArgumentException.class, () -> Hpke.x25519PublicFromNodeId("nothex"));
    assertThrows(IllegalArgumentException.class, () -> Hpke.x25519PublicFromIdentity(new byte[32]));
  }

  @Test
  void attestationMintVerifyRoles() {
    byte[] root = fill(1);
    byte[] node = fill(2);
    byte[] rootPk = Ids.publicKey(root);
    byte[] nodePk = Ids.publicKey(node);
    byte[] chain = Attestation.attestRootNode(root, node);
    String text = Attestation.toText(chain);
    assertTrue(text.startsWith("aster.attestation.chain.v1:"));
    assertArrayEquals(chain, Attestation.fromText(text));
    assertThrows(IllegalArgumentException.class, () -> Attestation.fromText("nope"));

    VerifiedChain v = Attestation.verifyChain(chain, List.of(fill(5), rootPk), nodePk);
    assertEquals(1, v.depth());
    assertArrayEquals(nodePk, v.node());
    assertArrayEquals(rootPk, v.anchor());
    assertTrue(v.intermediates().isEmpty());
    assertEquals(Optional.of(AttestationRole.NODE), v.roleOf(nodePk));
    assertEquals(Optional.of(AttestationRole.ANCHOR), v.roleOf(rootPk));
    assertEquals(Optional.empty(), v.roleOf(fill(5)));

    assertThrows(
        VerificationFailedException.class,
        () -> Attestation.verifyChain(chain, List.of(nodePk), nodePk));
    assertThrows(
        VerificationFailedException.class,
        () -> Attestation.verifyChain(chain, List.of(rootPk), rootPk));
    assertThrows(
        VerificationFailedException.class, () -> Attestation.verifyChain(chain, List.of(), nodePk));
    byte[] expired = Attestation.attestRootNode(root, node, new AttestOptions(1, 0, 1));
    VerificationFailedException e =
        assertThrows(
            VerificationFailedException.class,
            () -> Attestation.verifyChain(expired, List.of(rootPk), nodePk));
    assertTrue(e.getMessage().contains("expired"), e.getMessage());
  }
}
