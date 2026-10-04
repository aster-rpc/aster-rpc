package site.aster.grants;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.IOException;
import java.lang.foreign.MemoryLayout;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HexFormat;
import org.junit.jupiter.api.Test;
import site.aster.crypto.Hpke;
import site.aster.exception.VerificationFailedException;
import site.aster.id.IdKind;
import site.aster.id.Ids;

/**
 * {@link Grants} (C ABI 3.4) against the shared golden vectors {@code
 * conformance/vectors/grants.json}: AAD, grant keys, opening pinned envelopes and resealing, the
 * invalid opens, namespace grants and the role rule, and every reconcile outcome.
 */
final class GrantVectorsTest {

  private static final HexFormat HEX = HexFormat.of();
  private static final ObjectMapper JSON = new ObjectMapper();

  static JsonNode vectors() throws IOException {
    Path dir = Path.of("").toAbsolutePath();
    for (int i = 0; i < 8 && dir != null; i++, dir = dir.getParent()) {
      Path p = dir.resolve("conformance").resolve("vectors").resolve("grants.json");
      if (Files.exists(p)) {
        return JSON.readTree(p.toFile());
      }
    }
    throw new IllegalStateException("conformance/vectors/grants.json not found above cwd");
  }

  private static byte[] hex(JsonNode v, String k) {
    return HEX.parseHex(v.get(k).asText());
  }

  private static GrantContext ctx(JsonNode c) {
    return new GrantContext(
        c.get("app").asText(),
        c.get("granter").asText(),
        c.get("recipient").asText(),
        hex(c, "resource_hex"),
        c.get("path").asText(),
        c.get("role").asText());
  }

  private static byte[] openByScheme(JsonNode v, GrantContext c) {
    byte[] env = hex(v, "envelope_hex");
    byte[] key = hex(v, "recipient_key_hex");
    return switch (v.get("scheme").asText()) {
      case "identity" -> Grants.open(key, c, env);
      case "x25519" -> Grants.openWithKey(key, c, env);
      default -> throw new IllegalStateException(v.get("scheme").asText());
    };
  }

  private static long off(MemoryLayout l, String f) {
    return l.byteOffset(MemoryLayout.PathElement.groupElement(f));
  }

  @Test
  void layoutsMatchHeader() {
    assertEquals(104, Grants.GRANT_CONTEXT.byteSize());
    assertEquals(88, off(Grants.GRANT_CONTEXT, "role"));
    assertEquals(72, Grants.GRANT_RECORD.byteSize());
    assertEquals(48, off(Grants.GRANT_RECORD, "reason"));
    assertEquals(64, off(Grants.GRANT_RECORD, "storage"));
    assertEquals(72, Grants.GRANT_OUTCOME.byteSize());
    assertEquals(12, off(Grants.GRANT_OUTCOME, "drops_capability"));
    assertEquals(16, off(Grants.GRANT_OUTCOME, "payload"));
    assertEquals(64, off(Grants.GRANT_OUTCOME, "storage"));
  }

  @Test
  void aadVectors() throws IOException {
    for (JsonNode v : vectors().get("aad")) {
      GrantContext c = ctx(v.get("context"));
      assertEquals(v.get("aad_hex").asText(), HEX.formatHex(c.aad()), v.get("name").asText());
    }
  }

  @Test
  void grantKeyVectors() throws IOException {
    for (JsonNode v : vectors().get("grant_key")) {
      assertEquals(
          v.get("key").asText(),
          Grants.grantKey(v.get("prefix").asText(), v.get("recipient").asText()));
    }
  }

  @Test
  void openVectorsAndReseal() throws IOException {
    JsonNode doc = vectors();
    for (JsonNode v : doc.get("open")) {
      String name = v.get("name").asText();
      GrantContext c = ctx(v.get("context"));
      byte[] plaintext = openByScheme(v, c);
      assertEquals(v.get("plaintext_hex").asText(), HEX.formatHex(plaintext), name);
      byte[] key = hex(v, "recipient_key_hex");
      byte[] reopened;
      if (v.get("scheme").asText().equals("identity")) {
        reopened = Grants.open(key, c, Grants.seal(c, plaintext));
      } else {
        byte[] pub = Hpke.publicKeyFromPrivate(key);
        assertEquals(v.get("recipient_public_hex").asText(), HEX.formatHex(pub));
        reopened = Grants.openWithKey(key, c, Grants.sealToKey(pub, c, plaintext));
      }
      assertArrayEquals(plaintext, reopened, name);
    }
    for (JsonNode v : doc.get("open_invalid")) {
      GrantContext c = ctx(v.get("context"));
      assertThrows(
          VerificationFailedException.class, () -> openByScheme(v, c), v.get("name").asText());
    }
  }

  @Test
  void identitySchemeMatchesHpke() throws IOException {
    JsonNode v = vectors().get("open").get(0);
    GrantContext c = ctx(v.get("context"));
    byte[] identity = hex(v, "recipient_key_hex");
    byte[] env = hex(v, "envelope_hex");
    byte[] xSecret = Hpke.x25519SecretFromIdentity(identity);
    assertEquals(v.get("plaintext_hex").asText(), HEX.formatHex(Hpke.open(xSecret, c.aad(), env)));
    byte[] nodeId = Ids.parse(IdKind.NODE_ID, c.recipient());
    assertArrayEquals(Ids.publicKey(identity), nodeId);
    assertEquals(c.recipient(), Ids.nodeId(identity));
    assertArrayEquals(
        Hpke.publicKeyFromPrivate(xSecret), Hpke.x25519PublicFromNodeId(c.recipient()));
    assertArrayEquals(Hpke.publicKeyFromPrivate(xSecret), Hpke.x25519PublicFromIdentity(nodeId));
  }

  @Test
  void namespaceVectors() throws IOException {
    JsonNode doc = vectors();
    for (JsonNode v : doc.get("namespace_open")) {
      GrantContext c = ctx(v.get("context"));
      byte[] secret = hex(v, "recipient_secret_hex");
      NamespaceCapability cap = Grants.openNamespace(secret, c, hex(v, "envelope_hex"));
      assertEquals(v.get("can_write").asBoolean(), cap.canWrite());
      assertEquals(v.get("capability_hex").asText(), HEX.formatHex(cap.encode()));
      assertEquals(cap, NamespaceCapability.decode(hex(v, "capability_hex")));
      assertEquals(cap, Grants.openNamespace(secret, c, Grants.sealNamespace(c, cap)));
      NamespaceCapability flipped =
          cap.canWrite()
              ? NamespaceCapability.read(cap.namespaceId())
              : NamespaceCapability.write(new byte[32]);
      assertThrows(VerificationFailedException.class, () -> Grants.sealNamespace(c, flipped));
    }
    for (JsonNode v : doc.get("namespace_open_invalid")) {
      GrantContext c = ctx(v.get("context"));
      byte[] secret = hex(v, "recipient_secret_hex");
      byte[] env = hex(v, "envelope_hex");
      Grants.open(secret, c, env); // opens as a raw grant
      assertThrows(VerificationFailedException.class, () -> Grants.openNamespace(secret, c, env));
    }
    for (JsonNode v : doc.get("role_rule")) {
      assertEquals(
          v.get("ok").asBoolean(),
          Grants.roleMatches(v.get("role").asText(), v.get("can_write").asBoolean()),
          v.toString());
    }
  }

  @Test
  void reconcileVectors() throws IOException {
    for (JsonNode v : vectors().get("reconcile")) {
      String name = v.get("name").asText();
      GrantContext c = ctx(v.get("context"));
      byte[] secret = hex(v, "recipient_secret_hex");
      byte[] record = v.get("record_hex").isNull() ? null : hex(v, "record_hex");
      JsonNode expect = v.get("outcome");
      boolean namespace = v.get("payload").asText().equals("namespace");
      GrantOutcome<?> o =
          namespace
              ? Grants.reconcileNamespace(secret, c, record)
              : Grants.reconcile(secret, c, record);
      String kind = expect.get("kind").asText();
      assertEquals(kind.equals("revoked") || kind.equals("unopenable"), o.dropsCapability(), name);
      switch (kind) {
        case "active" -> {
          var a = assertInstanceOf(GrantOutcome.Active.class, o, name);
          if (namespace) {
            NamespaceCapability cap = (NamespaceCapability) a.value();
            assertEquals(expect.get("capability_hex").asText(), HEX.formatHex(cap.encode()), name);
          } else {
            assertEquals(
                expect.get("plaintext_hex").asText(), HEX.formatHex((byte[]) a.value()), name);
          }
        }
        case "revoked" -> {
          var r = assertInstanceOf(GrantOutcome.Revoked.class, o, name);
          assertEquals(expect.get("reason").asText(), r.reason(), name);
          assertEquals(expect.get("role").asText(), r.role(), name);
        }
        case "unopenable" -> {
          var u = assertInstanceOf(GrantOutcome.Unopenable.class, o, name);
          assertFalse(u.error().isEmpty(), name);
          if (expect.has("error_contains")) {
            assertTrue(u.error().contains(expect.get("error_contains").asText()), u.error());
          }
        }
        case "absent" -> assertInstanceOf(GrantOutcome.Absent.class, o, name);
        default -> throw new IllegalStateException(kind);
      }
    }
  }

  @Test
  void grantRecordsRoundTrip() throws IOException {
    JsonNode doc = vectors();
    for (JsonNode v : doc.get("reconcile")) {
      if (v.get("record_hex").isNull()) {
        continue;
      }
      byte[] bytes = hex(v, "record_hex");
      if (v.get("name").asText().equals("garbage-record")) {
        assertThrows(IllegalArgumentException.class, () -> GrantRecord.decode(bytes));
        continue;
      }
      GrantRecord r = GrantRecord.decode(bytes);
      assertArrayEquals(bytes, r.encode(), v.get("name").asText());
    }
    // Seal -> active record -> reconcile, then revoke.
    JsonNode v = doc.get("open").get(0);
    GrantContext c = ctx(v.get("context"));
    byte[] secret = hex(v, "recipient_key_hex");
    byte[] payload = "payload".getBytes(StandardCharsets.UTF_8);
    GrantRecord active = GrantRecord.active(c.role(), 2, Grants.seal(c, payload));
    GrantRecord back = GrantRecord.decode(active.encode());
    assertEquals(2, back.contextVersion());
    assertTrue(back.isActive());
    var a =
        assertInstanceOf(GrantOutcome.Active.class, Grants.reconcile(secret, c, active.encode()));
    assertArrayEquals(payload, (byte[]) a.value());
    GrantRecord revoked = GrantRecord.revoked(c.role(), "gone");
    var r =
        assertInstanceOf(GrantOutcome.Revoked.class, Grants.reconcile(secret, c, revoked.encode()));
    assertEquals("gone", r.reason());
    assertThrows(
        IllegalArgumentException.class,
        () -> GrantRecord.active(c.role(), 1, new byte[0]).encode());
  }
}
