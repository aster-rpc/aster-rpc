package site.aster.crypto;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.HexFormat;
import org.junit.jupiter.api.Test;
import site.aster.exception.VerificationFailedException;
import site.aster.id.IdKind;
import site.aster.id.Ids;

/** Ids and HPKE through the C ABI 3.4 pure functions. */
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
}
