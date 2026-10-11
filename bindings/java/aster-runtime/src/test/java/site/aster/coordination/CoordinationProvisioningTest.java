package site.aster.coordination;

import static org.junit.jupiter.api.Assertions.*;
import static site.aster.coordination.CoordinationProvisioning.*;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyFactory;
import java.security.Signature;
import java.security.spec.EdECPrivateKeySpec;
import java.security.spec.NamedParameterSpec;
import java.util.HexFormat;
import java.util.List;
import java.util.stream.StreamSupport;
import org.junit.jupiter.api.Test;

public final class CoordinationProvisioningTest {
  public static JsonNode vectors() throws Exception {
    Path root = Path.of("").toAbsolutePath();
    while (root != null
        && !Files.exists(root.resolve("tests/fixtures/coordination_provisioning_v1.json")))
      root = root.getParent();
    assertNotNull(root, "shared provisioning vector fixture");
    return new ObjectMapper()
        .readTree(root.resolve("tests/fixtures/coordination_provisioning_v1.json").toFile());
  }

  public static byte[] hex(String text) {
    return HexFormat.of().parseHex(text);
  }

  static byte[] b(JsonNode value, String field) {
    return hex(value.required(field).textValue());
  }

  static List<byte[]> ids(JsonNode value) {
    return StreamSupport.stream(value.spliterator(), false).map(v -> hex(v.textValue())).toList();
  }

  public static GenesisInput genesis(JsonNode value) {
    return new GenesisInput(
        b(value, "group_id"),
        value.required("authority_domain").textValue(),
        b(value, "genesis_nonce"),
        value.required("deployment_profile_code").intValue(),
        StreamSupport.stream(value.required("initial_voters").spliterator(), false)
            .map(v -> new VoterSeed(b(v, "node_id"), v.required("ticket").textValue()))
            .toList(),
        ids(value.required("initial_admins")),
        value.required("recovery_export_hash").isNull() ? null : b(value, "recovery_export_hash"));
  }

  static JoinPermitInput join(JsonNode value) {
    return new JoinPermitInput(
        b(value, "group_id"),
        value.required("authority_domain").textValue(),
        b(value, "genesis_hash"),
        b(value, "candidate_node_id"),
        ids(value.required("bootstrap_senders")),
        b(value, "join_nonce"));
  }

  static byte[] sign(byte[] seed, byte[] preimage) throws Exception {
    var key =
        KeyFactory.getInstance("Ed25519")
            .generatePrivate(new EdECPrivateKeySpec(NamedParameterSpec.ED25519, seed));
    Signature signer = Signature.getInstance("Ed25519");
    signer.initSign(key);
    signer.update(preimage);
    return signer.sign();
  }

  @Test
  void sharedGenesisVectorAndExternalJdkSignerMatchRustExactly() throws Exception {
    var vectors = vectors();
    var vector = vectors.required("genesis");
    var request = vector.required("request");
    byte[] document = canonicalGenesis(genesis(request));
    assertArrayEquals(b(vector, "document_bytes"), document);
    assertArrayEquals(b(vector, "signing_bytes"), genesisSigningBytes(document));
    byte[] signature =
        sign(b(vectors, "test_only_authority_secret"), genesisSigningBytes(document));
    assertArrayEquals(b(vector, "signature"), signature);
    byte[] envelope =
        signedDocumentFromParts(document, b(vectors, "authority_public_key"), signature);
    assertArrayEquals(b(vector, "signed_document"), envelope);
    assertArrayEquals(b(vector, "genesis_hash"), genesisHash(document));
    var verified =
        verifyGenesis(
            b(vectors, "authority_public_key"),
            request.required("authority_domain").textValue(),
            envelope);
    var expected = vector.required("verified");
    assertArrayEquals(b(expected, "document_bytes"), verified.documentBytes());
    assertArrayEquals(b(expected, "genesis_hash"), verified.genesisHash());
    assertArrayEquals(b(expected, "group_id"), verified.groupId());
    assertArrayEquals(b(expected, "authority_public_key"), verified.authorityPublicKey());
    assertEquals(expected.required("authority_domain").textValue(), verified.authorityDomain());
    assertEquals(
        expected.required("deployment_profile_code").intValue(), verified.deploymentProfileCode());
    assertEquals(expected.required("initial_voters").size(), verified.initialVoters().size());
    for (int i = 0; i < verified.initialVoters().size(); i++) {
      assertArrayEquals(
          b(expected.required("initial_voters").get(i), "node_id"),
          verified.initialVoters().get(i).nodeId());
      assertEquals(
          expected.required("initial_voters").get(i).required("ticket").textValue(),
          verified.initialVoters().get(i).ticket());
    }
    for (int i = 0; i < verified.initialAdmins().size(); i++)
      assertArrayEquals(
          hex(expected.required("initial_admins").get(i).textValue()),
          verified.initialAdmins().get(i));
    if (expected.required("recovery_export_hash").isNull())
      assertNull(verified.recoveryExportHash());
    else assertArrayEquals(b(expected, "recovery_export_hash"), verified.recoveryExportHash());
  }

  @Test
  void sharedJoinVectorDerivesTicketAndVerifiesCandidatePins() throws Exception {
    var vectors = vectors();
    var vector = vectors.required("join");
    var request = vector.required("request");
    byte[] document = canonicalJoinPermit(join(request));
    assertArrayEquals(b(vector, "document_bytes"), document);
    assertEquals(
        vector.required("candidate_ticket").textValue(),
        canonicalCandidateTicket(b(request, "candidate_node_id")));
    assertArrayEquals(b(vector, "signing_bytes"), joinSigningBytes(document));
    byte[] signature = sign(b(vectors, "test_only_authority_secret"), joinSigningBytes(document));
    assertArrayEquals(b(vector, "signature"), signature);
    byte[] envelope =
        signedDocumentFromParts(document, b(vectors, "authority_public_key"), signature);
    assertArrayEquals(b(vector, "signed_document"), envelope);
    var verified =
        verifyJoinPermitForStart(
            b(vectors, "authority_public_key"),
            request.required("authority_domain").textValue(),
            b(request, "candidate_node_id"),
            envelope);
    var expected = vector.required("verified");
    assertArrayEquals(b(expected, "document_bytes"), verified.documentBytes());
    assertArrayEquals(b(expected, "group_id"), verified.groupId());
    assertArrayEquals(b(expected, "genesis_hash"), verified.genesisHash());
    assertArrayEquals(b(expected, "candidate_node_id"), verified.candidateNodeId());
    for (int i = 0; i < verified.bootstrapSenders().size(); i++)
      assertArrayEquals(
          hex(expected.required("bootstrap_senders").get(i).textValue()),
          verified.bootstrapSenders().get(i));
    byte[] wrongCandidate = b(request, "candidate_node_id");
    wrongCandidate[0] ^= 1;
    assertEquals(
        "wrong_node",
        assertThrows(
                ProvisioningException.class,
                () ->
                    verifyJoinPermitForStart(
                        b(vectors, "authority_public_key"),
                        request.required("authority_domain").textValue(),
                        wrongCandidate,
                        envelope))
            .code());
  }

  @Test
  void verificationRejectsWrongPinsTamperingAndCrossDomainSignature() throws Exception {
    var vectors = vectors();
    var vector = vectors.required("genesis");
    byte[] key = b(vectors, "authority_public_key");
    byte[] document = b(vector, "document_bytes");
    byte[] envelope = b(vector, "signed_document");
    String domain = vector.required("request").required("authority_domain").textValue();
    assertEquals(
        "wrong_domain",
        assertThrows(ProvisioningException.class, () -> verifyGenesis(key, "elsewhere", envelope))
            .code());
    byte[] wrongKey = key.clone();
    wrongKey[0] ^= 1;
    assertEquals(
        "wrong_authority_key",
        assertThrows(ProvisioningException.class, () -> verifyGenesis(wrongKey, domain, envelope))
            .code());
    byte[] bad = b(vector, "signature");
    bad[0] ^= 1;
    assertEquals(
        "bad_signature",
        assertThrows(
                ProvisioningException.class,
                () -> verifyGenesis(key, domain, signedDocumentFromParts(document, key, bad)))
            .code());
    byte[] cross = sign(b(vectors, "test_only_authority_secret"), joinSigningBytes(document));
    assertEquals(
        "bad_signature",
        assertThrows(
                ProvisioningException.class,
                () -> verifyGenesis(key, domain, signedDocumentFromParts(document, key, cross)))
            .code());
    assertEquals(
        "malformed",
        assertThrows(ProvisioningException.class, () -> verifyGenesis(key, domain, new byte[0]))
            .code());
  }

  @Test
  void widthsAndProfileErrorsAreTypedAndInputsOutputsAreDefensive() throws Exception {
    var vectors = vectors();
    var vector = vectors.required("genesis");
    var input = genesis(vector.required("request"));
    assertEquals(
        "invalid_input",
        assertThrows(
                ProvisioningException.class,
                () -> signedDocumentFromParts(new byte[0], new byte[31], new byte[64]))
            .code());
    assertEquals(
        "invalid_input",
        assertThrows(
                ProvisioningException.class,
                () -> signedDocumentFromParts(new byte[0], new byte[32], new byte[63]))
            .code());
    assertEquals(
        "invalid_input",
        assertThrows(ProvisioningException.class, () -> canonicalCandidateTicket(new byte[31]))
            .code());
    var unknownProfile =
        new GenesisInput(
            input.groupId(),
            input.authorityDomain(),
            input.genesisNonce(),
            999,
            input.initialVoters(),
            input.initialAdmins(),
            input.recoveryExportHash());
    byte[] document = canonicalGenesis(unknownProfile);
    byte[] envelope =
        signedDocumentFromParts(
            document,
            b(vectors, "authority_public_key"),
            sign(b(vectors, "test_only_authority_secret"), genesisSigningBytes(document)));
    assertEquals(
        "unknown_profile",
        assertThrows(
                ProvisioningException.class,
                () ->
                    verifyGenesis(
                        b(vectors, "authority_public_key"), input.authorityDomain(), envelope))
            .code());
    input.groupId()[0] ^= 1;
    input.initialAdmins().getFirst()[0] ^= 1;
    input.initialVoters().getFirst().nodeId()[0] ^= 1;
    assertArrayEquals(b(vector, "document_bytes"), canonicalGenesis(input));
    var verified =
        verifyGenesis(
            b(vectors, "authority_public_key"),
            input.authorityDomain(),
            b(vector, "signed_document"));
    verified.documentBytes()[0] ^= 1;
    verified.initialAdmins().getFirst()[0] ^= 1;
    assertArrayEquals(b(vector, "document_bytes"), verified.documentBytes());
    assertArrayEquals(input.initialAdmins().getFirst(), verified.initialAdmins().getFirst());
  }
}
