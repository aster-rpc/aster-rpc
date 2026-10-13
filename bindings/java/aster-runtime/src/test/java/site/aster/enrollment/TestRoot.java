package site.aster.enrollment;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.charset.StandardCharsets;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.PrivateKey;
import java.security.SecureRandom;
import java.security.Signature;
import java.security.spec.NamedParameterSpec;
import java.time.Duration;
import java.util.Arrays;
import java.util.Base64;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Plays an Enrollment root in tests (Go's {@code internal/testroot}): signs the credentials and
 * delegations {@code aster enroll node} / {@code aster enroll delegate} produce (compact Ed25519
 * JWT delegation chains) and builds the revocation record {@code aster enroll revoke} publishes.
 * Not part of the binding: root operations belong to the {@code aster} CLI. Rust verifying links
 * signed here doubles as a cross-implementation check.
 */
public final class TestRoot {

  private static final Base64.Encoder B64 = Base64.getUrlEncoder().withoutPadding();
  private static final ObjectMapper MAPPER = new ObjectMapper();
  private static final SecureRandom RANDOM = new SecureRandom();

  private final PrivateKey key;
  private final byte[] publicKey;
  private final String audience;

  /** A root from a 32-byte seed, for one fleet audience. */
  public TestRoot(byte[] seed, String audience) {
    KeyPair pair = keyPair(seed);
    this.key = pair.getPrivate();
    this.publicKey = raw(pair);
    this.audience = audience;
  }

  /** A root with a fresh random key. */
  public static TestRoot generate(String audience) {
    byte[] seed = new byte[32];
    RANDOM.nextBytes(seed);
    return new TestRoot(seed, audience);
  }

  /** The root public key, 64 hex ({@code [trust] root_pubkey}). */
  public String publicKeyHex() {
    return HexFormat.of().formatHex(publicKey);
  }

  /** The root public key bytes. */
  public byte[] publicKey() {
    return publicKey.clone();
  }

  /** The node id (64 hex) of a 32-byte secret key. */
  public static String nodeId(byte[] secret) {
    return HexFormat.of().formatHex(raw(keyPair(secret)));
  }

  /** The fleet namespace id (64 hex) of a namespace secret. */
  public static String namespace(byte[] secret) {
    return nodeId(secret);
  }

  /** The fleet-namespace key of {@code subject}'s registration record. */
  public static String registrationKey(String subject) {
    return "aster/enrollment/v1/nodes/" + subject;
  }

  /**
   * Claims carrying {@code roles} in {@code aster.role} (what {@code aster enroll node --attributes
   * roles.json} writes). Empty roles: no attribute.
   */
  public static Map<String, Object> roleClaims(String roles) {
    Map<String, Object> attributes = new LinkedHashMap<>();
    if (!roles.isEmpty()) {
      attributes.put("aster.role", Map.of("type", "string", "value", roles));
    }
    Map<String, Object> claims = new LinkedHashMap<>();
    claims.put("scopes", List.of());
    claims.put("tags", List.of());
    claims.put("attributes", attributes);
    return claims;
  }

  /** A one-link, root-issued, ephemeral credential chain for {@code subject}. */
  public String credential(String subject, Map<String, Object> claims, Duration lifetime) {
    Map<String, Object> body = new LinkedHashMap<>(claims);
    body.put("mandatory", List.of());
    body.put("registration", "ephemeral");
    return "aster-chain-v1~"
        + link("aster-enrollment-v1+jwt", subject, lifetime, "aster_enrollment_v1", body);
  }

  /** A delegation letting {@code subject} enroll ephemeral nodes carrying {@code attributeKeys}. */
  public String delegation(
      String subject, List<String> scopes, List<String> attributeKeys, Duration lifetime) {
    Map<String, Object> body = new LinkedHashMap<>();
    body.put("may_issue", List.of("enrollment"));
    body.put("audiences", List.of(audience));
    body.put("scopes", scopes);
    body.put("tags", List.of());
    body.put("attribute_keys", attributeKeys);
    body.put("max_child_lifetime", 3600);
    body.put("max_depth", 0);
    body.put("subjects", null);
    body.put("operations", List.of());
    body.put("mandatory", List.of());
    body.put("may_register", false);
    return "aster-chain-v1~"
        + link("aster-delegation-v1+jwt", subject, lifetime, "aster_delegation_v1", body);
  }

  /** The root {@code NodeRegistrationV1} record revoking {@code subject}. */
  public byte[] revocation(String subject, String reason) {
    Map<String, Object> r = new LinkedHashMap<>();
    r.put("version", 1);
    r.put("issuer", publicKeyHex());
    r.put("subject", subject);
    r.put("audience", audience);
    r.put("revision", 1);
    r.put("issued_at", System.currentTimeMillis() / 1000 - 60);
    r.put("status", "revoked");
    r.put("claims", Map.of("scopes", List.of(), "tags", List.of(), "attributes", Map.of()));
    r.put("mandatory", List.of());
    r.put("reauthorizes_revision", null);
    r.put("reason", reason);
    return json(r);
  }

  private String link(
      String typ, String subject, Duration lifetime, String extension, Object body) {
    byte[] sub = HexFormat.of().parseHex(subject);
    byte[] jti = new byte[16];
    RANDOM.nextBytes(jti);
    long now = System.currentTimeMillis() / 1000;
    Map<String, Object> payload = new LinkedHashMap<>();
    payload.put("iss", publicKeyHex());
    payload.put("sub", subject);
    payload.put(
        "cnf", Map.of("jwk", Map.of("kty", "OKP", "crv", "Ed25519", "x", B64.encodeToString(sub))));
    payload.put("aud", audience);
    payload.put("jti", B64.encodeToString(jti));
    payload.put("iat", now);
    payload.put("exp", now + lifetime.toSeconds());
    payload.put(extension, body);
    String input =
        B64.encodeToString(json(Map.of("alg", "EdDSA", "typ", typ)))
            + "."
            + B64.encodeToString(json(payload));
    try {
      Signature s = Signature.getInstance("Ed25519");
      s.initSign(key);
      s.update(input.getBytes(StandardCharsets.US_ASCII));
      return input + "." + B64.encodeToString(s.sign());
    } catch (java.security.GeneralSecurityException e) {
      throw new IllegalStateException(e);
    }
  }

  private static byte[] json(Object v) {
    try {
      return MAPPER.writeValueAsBytes(v);
    } catch (com.fasterxml.jackson.core.JsonProcessingException e) {
      throw new IllegalStateException(e);
    }
  }

  /** An Ed25519 key pair from a 32-byte seed (the generator draws exactly the seed). */
  private static KeyPair keyPair(byte[] seed) {
    try {
      KeyPairGenerator g = KeyPairGenerator.getInstance("Ed25519");
      g.initialize(
          NamedParameterSpec.ED25519,
          new SecureRandom() {
            private static final long serialVersionUID = 1L;

            @Override
            public void nextBytes(byte[] bytes) {
              System.arraycopy(seed, 0, bytes, 0, bytes.length);
            }
          });
      return g.generateKeyPair();
    } catch (java.security.GeneralSecurityException e) {
      throw new IllegalStateException(e);
    }
  }

  /** The raw 32-byte public key (the tail of the X.509 encoding). */
  private static byte[] raw(KeyPair pair) {
    byte[] der = pair.getPublic().getEncoded();
    return Arrays.copyOfRange(der, der.length - 32, der.length);
  }
}
