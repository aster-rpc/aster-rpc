package site.aster.coordination;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import site.aster.ffi.IrohException;
import site.aster.ffi.NativeCall;

/**
 * Offline coordination provisioning using Rust's canonical encoding and verification (ABI 4.15).
 * Authority private keys stay with the caller's external signer.
 */
public final class CoordinationProvisioning {
  private CoordinationProvisioning() {}

  private static final ObjectMapper JSON = new ObjectMapper();
  private static final HexFormat HEX = HexFormat.of();
  private static final MemoryLayout RESULT =
      MemoryLayout.structLayout(NativeCall.BYTES, NativeCall.BYTES);

  private static MethodHandle function(String name, int inputs) {
    MemoryLayout[] arguments = new MemoryLayout[inputs + 1];
    Arrays.fill(arguments, NativeCall.BYTES);
    arguments[inputs] = ValueLayout.ADDRESS;
    return NativeCall.handle("aster_coordination_" + name, NativeCall.intFn(arguments));
  }

  private static final class H {
    static final MethodHandle GENESIS = function("canonical_genesis", 1);
    static final MethodHandle JOIN = function("canonical_join_permit", 1);
    static final MethodHandle GENESIS_SIGNING = function("genesis_signing_bytes", 1);
    static final MethodHandle JOIN_SIGNING = function("join_signing_bytes", 1);
    static final MethodHandle HASH = function("genesis_hash", 1);
    static final MethodHandle ENVELOPE = function("signed_document_from_parts", 3);
    static final MethodHandle TICKET = function("canonical_candidate_ticket", 1);
    static final MethodHandle VERIFY_GENESIS = function("verify_genesis", 3);
    static final MethodHandle VERIFY_JOIN = function("verify_join_permit_for_start", 4);
  }

  /**
   * Stable native reason and diagnostic detail, without collapsing policy errors into signature
   * errors.
   */
  public static final class ProvisioningException extends RuntimeException {
    private final String code;
    private final String detail;

    private ProvisioningException(String code, String detail) {
      super(code + ": " + detail);
      this.code = code;
      this.detail = detail;
    }

    public String code() {
      return code;
    }

    public String detail() {
      return detail;
    }
  }

  public record VoterSeed(byte[] nodeId, String ticket) {
    public VoterSeed {
      nodeId = nodeId.clone();
      Objects.requireNonNull(ticket, "ticket");
    }

    @Override
    public byte[] nodeId() {
      return nodeId.clone();
    }
  }

  /** Version is fixed to 1. Lists must already follow the Rust protocol's ordering rules. */
  public record GenesisInput(
      byte[] groupId,
      String authorityDomain,
      byte[] genesisNonce,
      int deploymentProfileCode,
      List<VoterSeed> initialVoters,
      List<byte[]> initialAdmins,
      byte[] recoveryExportHash) {
    public GenesisInput {
      groupId = groupId.clone();
      genesisNonce = genesisNonce.clone();
      Objects.requireNonNull(authorityDomain, "authorityDomain");
      initialVoters = List.copyOf(initialVoters);
      initialAdmins = copies(initialAdmins);
      recoveryExportHash = copyOrNull(recoveryExportHash);
    }

    @Override
    public byte[] groupId() {
      return groupId.clone();
    }

    @Override
    public byte[] genesisNonce() {
      return genesisNonce.clone();
    }

    @Override
    public List<byte[]> initialAdmins() {
      return copies(initialAdmins);
    }

    @Override
    public byte[] recoveryExportHash() {
      return copyOrNull(recoveryExportHash);
    }
  }

  /** Rust derives the canonical addressless candidate ticket; no caller ticket is accepted. */
  public record JoinPermitInput(
      byte[] groupId,
      String authorityDomain,
      byte[] genesisHash,
      byte[] candidateNodeId,
      List<byte[]> bootstrapSenders,
      byte[] joinNonce) {
    public JoinPermitInput {
      groupId = groupId.clone();
      genesisHash = genesisHash.clone();
      candidateNodeId = candidateNodeId.clone();
      joinNonce = joinNonce.clone();
      Objects.requireNonNull(authorityDomain, "authorityDomain");
      bootstrapSenders = copies(bootstrapSenders);
    }

    @Override
    public byte[] groupId() {
      return groupId.clone();
    }

    @Override
    public byte[] genesisHash() {
      return genesisHash.clone();
    }

    @Override
    public byte[] candidateNodeId() {
      return candidateNodeId.clone();
    }

    @Override
    public List<byte[]> bootstrapSenders() {
      return copies(bootstrapSenders);
    }

    @Override
    public byte[] joinNonce() {
      return joinNonce.clone();
    }
  }

  /**
   * Verified metadata from Rust, not a transferable authorization capability or native sealed
   * value.
   */
  public record VerifiedGenesis(
      byte[] documentBytes,
      byte[] genesisHash,
      byte[] groupId,
      byte[] authorityPublicKey,
      String authorityDomain,
      int deploymentProfileCode,
      List<VoterSeed> initialVoters,
      List<byte[]> initialAdmins,
      byte[] recoveryExportHash) {
    public VerifiedGenesis {
      documentBytes = documentBytes.clone();
      genesisHash = genesisHash.clone();
      groupId = groupId.clone();
      authorityPublicKey = authorityPublicKey.clone();
      initialVoters = List.copyOf(initialVoters);
      initialAdmins = copies(initialAdmins);
      recoveryExportHash = copyOrNull(recoveryExportHash);
    }

    @Override
    public byte[] documentBytes() {
      return documentBytes.clone();
    }

    @Override
    public byte[] genesisHash() {
      return genesisHash.clone();
    }

    @Override
    public byte[] groupId() {
      return groupId.clone();
    }

    @Override
    public byte[] authorityPublicKey() {
      return authorityPublicKey.clone();
    }

    @Override
    public List<byte[]> initialAdmins() {
      return copies(initialAdmins);
    }

    @Override
    public byte[] recoveryExportHash() {
      return copyOrNull(recoveryExportHash);
    }
  }

  /** Group/genesis are extracted, not compared to an independent expected group/genesis. */
  public record VerifiedJoinPermit(
      byte[] documentBytes,
      byte[] groupId,
      byte[] genesisHash,
      byte[] candidateNodeId,
      List<byte[]> bootstrapSenders) {
    public VerifiedJoinPermit {
      documentBytes = documentBytes.clone();
      groupId = groupId.clone();
      genesisHash = genesisHash.clone();
      candidateNodeId = candidateNodeId.clone();
      bootstrapSenders = copies(bootstrapSenders);
    }

    @Override
    public byte[] documentBytes() {
      return documentBytes.clone();
    }

    @Override
    public byte[] groupId() {
      return groupId.clone();
    }

    @Override
    public byte[] genesisHash() {
      return genesisHash.clone();
    }

    @Override
    public byte[] candidateNodeId() {
      return candidateNodeId.clone();
    }

    @Override
    public List<byte[]> bootstrapSenders() {
      return copies(bootstrapSenders);
    }
  }

  private static byte[] copyOrNull(byte[] value) {
    return value == null ? null : value.clone();
  }

  private static List<byte[]> copies(List<byte[]> values) {
    return values.stream().map(byte[]::clone).toList();
  }

  private static String hex(byte[] bytes) {
    return HEX.formatHex(bytes);
  }

  private static List<String> hexes(List<byte[]> bytes) {
    return bytes.stream().map(CoordinationProvisioning::hex).toList();
  }

  private static Map<String, Object> voter(VoterSeed seed) {
    return Map.of("node_id", hex(seed.nodeId()), "ticket", seed.ticket());
  }

  private static byte[] json(Map<String, Object> value) {
    try {
      return JSON.writeValueAsBytes(value);
    } catch (java.io.IOException e) {
      throw new IllegalArgumentException("cannot encode provisioning input", e);
    }
  }

  public static byte[] canonicalGenesis(GenesisInput input) {
    var value = new LinkedHashMap<String, Object>();
    value.put("group_id", hex(input.groupId()));
    value.put("authority_domain", input.authorityDomain());
    value.put("genesis_nonce", hex(input.genesisNonce()));
    value.put("deployment_profile_code", input.deploymentProfileCode());
    value.put(
        "initial_voters",
        input.initialVoters().stream().map(CoordinationProvisioning::voter).toList());
    value.put("initial_admins", hexes(input.initialAdmins()));
    value.put(
        "recovery_export_hash",
        input.recoveryExportHash() == null ? null : hex(input.recoveryExportHash()));
    return call(H.GENESIS, json(value));
  }

  public static byte[] canonicalJoinPermit(JoinPermitInput input) {
    return call(
        H.JOIN,
        json(
            Map.of(
                "group_id",
                hex(input.groupId()),
                "authority_domain",
                input.authorityDomain(),
                "genesis_hash",
                hex(input.genesisHash()),
                "candidate_node_id",
                hex(input.candidateNodeId()),
                "bootstrap_senders",
                hexes(input.bootstrapSenders()),
                "join_nonce",
                hex(input.joinNonce()))));
  }

  public static byte[] genesisSigningBytes(byte[] document) {
    return call(H.GENESIS_SIGNING, document);
  }

  public static byte[] joinSigningBytes(byte[] document) {
    return call(H.JOIN_SIGNING, document);
  }

  /** Hashes supplied bytes without verifying them. Only verifyGenesis establishes authority. */
  public static byte[] genesisHash(byte[] document) {
    return call(H.HASH, document);
  }

  public static byte[] signedDocumentFromParts(
      byte[] document, byte[] authorityKey, byte[] externalSignature) {
    return call(H.ENVELOPE, document, authorityKey, externalSignature);
  }

  public static String canonicalCandidateTicket(byte[] candidate) {
    return new String(call(H.TICKET, candidate), StandardCharsets.UTF_8);
  }

  /** Verify against independently trusted key/domain pins, never just the envelope's own key. */
  public static VerifiedGenesis verifyGenesis(byte[] authorityKey, String domain, byte[] envelope) {
    JsonNode value =
        tree(
            call(
                H.VERIFY_GENESIS, authorityKey, domain.getBytes(StandardCharsets.UTF_8), envelope));
    return new VerifiedGenesis(
        bytes(value, "document_bytes", -1),
        bytes(value, "genesis_hash", 32),
        bytes(value, "group_id", 32),
        bytes(value, "authority_public_key", 32),
        text(value, "authority_domain"),
        value.required("deployment_profile_code").intValue(),
        voters(value.required("initial_voters")),
        ids(value.required("initial_admins")),
        value.required("recovery_export_hash").isNull()
            ? null
            : bytes(value, "recovery_export_hash", 32));
  }

  /**
   * Checks key/domain/candidate. Group and genesis are learned from this permit, not independently
   * pinned.
   */
  public static VerifiedJoinPermit verifyJoinPermitForStart(
      byte[] authorityKey, String domain, byte[] candidate, byte[] envelope) {
    JsonNode value =
        tree(
            call(
                H.VERIFY_JOIN,
                authorityKey,
                domain.getBytes(StandardCharsets.UTF_8),
                candidate,
                envelope));
    return new VerifiedJoinPermit(
        bytes(value, "document_bytes", -1),
        bytes(value, "group_id", 32),
        bytes(value, "genesis_hash", 32),
        bytes(value, "candidate_node_id", 32),
        ids(value.required("bootstrap_senders")));
  }

  private static JsonNode tree(byte[] bytes) {
    try {
      return JSON.readTree(bytes);
    } catch (java.io.IOException e) {
      throw new IrohException("invalid provisioning JSON output: " + e.getMessage());
    }
  }

  private static String text(JsonNode value, String field) {
    JsonNode item = value.required(field);
    if (!item.isTextual()) throw new IrohException("invalid provisioning field " + field);
    return item.textValue();
  }

  private static byte[] bytes(JsonNode value, String field, int width) {
    byte[] bytes = HEX.parseHex(text(value, field));
    if (width >= 0 && bytes.length != width)
      throw new IrohException("invalid provisioning field width " + field);
    return bytes;
  }

  private static List<VoterSeed> voters(JsonNode value) {
    return java.util.stream.StreamSupport.stream(value.spliterator(), false)
        .map(v -> new VoterSeed(bytes(v, "node_id", 32), text(v, "ticket")))
        .toList();
  }

  private static List<byte[]> ids(JsonNode value) {
    return java.util.stream.StreamSupport.stream(value.spliterator(), false)
        .map(
            v -> {
              byte[] bytes = HEX.parseHex(v.textValue());
              if (bytes.length != 32) throw new IrohException("invalid provisioning id width");
              return bytes;
            })
        .toList();
  }

  private static byte[] call(MethodHandle function, byte[]... inputs) {
    try (Arena arena = Arena.ofConfined()) {
      var out = arena.allocate(RESULT);
      Object[] arguments = new Object[inputs.length + 1];
      for (int i = 0; i < inputs.length; i++) arguments[i] = NativeCall.bytes(arena, inputs[i]);
      arguments[inputs.length] = out;
      int status = NativeCall.call(function, arguments);
      byte[] data;
      byte[] error;
      try {
        data = NativeCall.takeOwned(out.asSlice(0, 16));
      } finally {
        error = NativeCall.takeOwned(out.asSlice(16, 16));
      }
      if (error.length != 0) {
        JsonNode value = tree(error);
        throw new ProvisioningException(text(value, "code"), text(value, "detail"));
      }
      NativeCall.check(status, "coordination provisioning");
      return data;
    }
  }
}
