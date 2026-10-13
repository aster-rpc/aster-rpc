package site.aster.enrollment;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import site.aster.config.AsterConfig;

/**
 * The {@code [trust]} + {@code [enrollment]} JSON of the C ABI ({@code
 * aster_config_enrollment_from_*}, {@code aster_node_start_enrolled}): the field names of the Rust
 * {@code AsterConfig} / {@code EnrollmentConfig} (design §15.3). Binding plumbing; applications use
 * {@link AsterConfig}.
 */
public final class TrustJson {

  static final ObjectMapper MAPPER = new ObjectMapper();

  private TrustJson() {}

  /**
   * The trust JSON of {@code config}, with the builder overrides {@code policy} and {@code roles}
   * (null = the configured ones).
   */
  public static byte[] encode(AsterConfig config, Policy policy, RoleMapping roles) {
    ObjectNode t = MAPPER.createObjectNode();
    byte[] root = config.rootPubkey();
    if (root != null && root.length > 0) {
      if (root.length != 32) {
        throw new IllegalArgumentException(
            "rootPubkey: " + root.length + " bytes, want 32 (an Ed25519 public key)");
      }
      t.put("root_pubkey", HexFormat.of().formatHex(root));
    }
    putText(t, "root_pubkey_file", config.rootPubkeyFile());
    putText(t, "next_root_commitment", config.nextRootCommitment());
    t.put("root_seq", config.rootSeq());
    ArrayNode rotations = t.putArray("root_rotations");
    config.rootRotations().forEach(rotations::add);
    putText(t, "enrollment_credential_file", config.enrollmentCredentialFile());
    putText(t, "enrollment_credential_chain", config.enrollmentCredentialChain());

    EnrollmentConfig e = config.enrollment();
    ObjectNode n = t.putObject("enrollment");
    putBool(n, "enabled", e.enabled());
    putText(n, "audience", e.audience());
    putText(n, "fleet_namespace", e.fleetNamespace());
    putText(n, "state_path", e.statePath());
    ArrayNode replicas = n.putArray("replicas");
    e.replicas().forEach(replicas::add);
    Policy p = policy != null ? policy : e.policy();
    if (p != null) {
      n.set("policy", p.toJsonNode());
    }
    n.put("require_synchronized_clock", e.requireSynchronizedClock());
    putLong(n, "activation_leeway_secs", e.activationLeewaySecs());
    EnrollmentLimits l = e.limits();
    ObjectNode limits = n.putObject("limits");
    putLong(limits, "handshake_timeout_ms", l.handshakeTimeoutMs());
    putLong(limits, "step_timeout_ms", l.stepTimeoutMs());
    putLong(limits, "policy_timeout_ms", l.policyTimeoutMs());
    putLong(limits, "reconciliation_interval_ms", l.reconciliationIntervalMs());
    putLong(limits, "max_admitted_peers", l.maxAdmittedPeers());
    putLong(limits, "sync_timeout_ms", l.syncTimeoutMs());
    RoleMapping r = roles != null ? roles : e.roles();
    ObjectNode rn = n.putObject("roles");
    if (r.attribute() != null) {
      rn.put("attribute", r.attribute());
    }
    rn.put("scopes_as_roles", r.scopesAsRoles());
    rn.set("scopes", MAPPER.valueToTree(r.scopes()));
    rn.set("tags", MAPPER.valueToTree(r.tags()));
    putBool(rn, "include_attributes", r.includeAttributes());
    putText(n, "issuer_delegation_file", e.issuerDelegationFile());
    putLong(n, "issuer_lifetime_secs", e.issuerLifetimeSecs());
    putText(n, "renew_from", e.renewFrom());
    putBool(n, "renew", e.renew());
    putBool(n, "synchronize", e.synchronize());
    try {
      return MAPPER.writeValueAsBytes(t);
    } catch (JsonProcessingException ex) {
      throw new IllegalStateException(ex);
    }
  }

  /** Set the trust and enrollment fields of {@code b} from the trust JSON Rust resolved. */
  public static AsterConfig.Builder apply(byte[] json, AsterConfig.Builder b) {
    JsonNode t;
    try {
      t = MAPPER.readTree(json);
    } catch (java.io.IOException ex) {
      throw new IllegalStateException("enrollment config: malformed native JSON", ex);
    }
    String root = text(t, "root_pubkey");
    b.rootPubkey(root == null ? null : HexFormat.of().parseHex(root));
    b.rootPubkeyFile(text(t, "root_pubkey_file"));
    b.rootRotation(
        text(t, "next_root_commitment"),
        t.path("root_seq").asLong(0),
        strings(t, "root_rotations"));
    b.enrollmentCredentialFile(text(t, "enrollment_credential_file"));
    b.enrollmentCredentialChain(text(t, "enrollment_credential_chain"));
    JsonNode n = t.path("enrollment");
    JsonNode l = n.path("limits");
    JsonNode r = n.path("roles");
    JsonNode policy = n.get("policy");
    b.enrollment(
        EnrollmentConfig.builder()
            .enabled(bool(n, "enabled"))
            .audience(text(n, "audience"))
            .fleetNamespace(text(n, "fleet_namespace"))
            .statePath(text(n, "state_path"))
            .replicas(strings(n, "replicas"))
            .policy(policy == null || policy.isNull() ? null : Policy.of(policy))
            .requireSynchronizedClock(n.path("require_synchronized_clock").asBoolean(false))
            .activationLeewaySecs(number(n, "activation_leeway_secs"))
            .limits(
                new EnrollmentLimits(
                    number(l, "handshake_timeout_ms"),
                    number(l, "step_timeout_ms"),
                    number(l, "policy_timeout_ms"),
                    number(l, "reconciliation_interval_ms"),
                    number(l, "max_admitted_peers"),
                    number(l, "sync_timeout_ms")))
            .roles(
                new RoleMapping(
                    text(r, "attribute"),
                    r.path("scopes_as_roles").asBoolean(false),
                    roleMap(r.path("scopes")),
                    roleMap(r.path("tags")),
                    bool(r, "include_attributes")))
            .issuerDelegationFile(text(n, "issuer_delegation_file"))
            .issuerLifetimeSecs(number(n, "issuer_lifetime_secs"))
            .renewFrom(text(n, "renew_from"))
            .renew(bool(n, "renew"))
            .synchronize(bool(n, "synchronize"))
            .build());
    return b;
  }

  private static void putText(ObjectNode n, String key, String v) {
    if (v != null && !v.isEmpty()) {
      n.put(key, v);
    }
  }

  private static void putLong(ObjectNode n, String key, Long v) {
    if (v != null) {
      n.put(key, v);
    }
  }

  private static void putBool(ObjectNode n, String key, Boolean v) {
    if (v != null) {
      n.put(key, v);
    }
  }

  private static String text(JsonNode n, String key) {
    JsonNode v = n.get(key);
    return v == null || v.isNull() ? null : v.asText();
  }

  private static Long number(JsonNode n, String key) {
    JsonNode v = n.get(key);
    return v == null || v.isNull() ? null : v.asLong();
  }

  private static Boolean bool(JsonNode n, String key) {
    JsonNode v = n.get(key);
    return v == null || v.isNull() ? null : v.asBoolean();
  }

  private static List<String> strings(JsonNode n, String key) {
    List<String> out = new ArrayList<>();
    n.path(key).forEach(v -> out.add(v.asText()));
    return out;
  }

  private static Map<String, List<String>> roleMap(JsonNode n) {
    Map<String, List<String>> out = new LinkedHashMap<>();
    n.fields()
        .forEachRemaining(
            e -> {
              List<String> roles = new ArrayList<>();
              e.getValue().forEach(v -> roles.add(v.asText()));
              out.put(e.getKey(), roles);
            });
    return out;
  }
}
