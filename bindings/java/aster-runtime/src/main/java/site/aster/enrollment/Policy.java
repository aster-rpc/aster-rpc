package site.aster.enrollment;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.JsonNodeFactory;
import com.fasterxml.jackson.databind.node.ObjectNode;
import com.fasterxml.jackson.databind.node.TextNode;
import java.time.Duration;

/**
 * A declarative admission policy, evaluated in Rust ({@code PolicyConfig}, design §15.5): the
 * externally tagged JSON tree of {@code all_of} / {@code any_of} / {@code none_of}, {@code
 * source_cidr}, {@code audience}, {@code scope}, {@code tag}, {@code attribute_equals}, {@code
 * issued_by}, {@code max_credential_age_secs}, {@code policy_synchronized_within} and the bare
 * strings {@code "tolerate_stale_policy"} and {@code "require_registration"}. Build one with the
 * static constructors, or set it in TOML ({@code [enrollment] policy}) or {@code
 * ASTER_ENROLLMENT_POLICY} (JSON). Rust validates the tree before the node starts.
 */
public final class Policy {

  private static final JsonNodeFactory F = JsonNodeFactory.instance;
  private static final ObjectMapper MAPPER = new ObjectMapper();

  private final JsonNode tree;

  private Policy(JsonNode tree) {
    this.tree = tree;
  }

  /** A policy from its JSON tree (validated by Rust when the node starts). */
  public static Policy fromJson(String json) {
    try {
      return new Policy(MAPPER.readTree(json));
    } catch (JsonProcessingException e) {
      throw new IllegalArgumentException("enrollment policy: not JSON: " + e.getOriginalMessage());
    }
  }

  static Policy of(JsonNode tree) {
    return new Policy(tree.deepCopy());
  }

  /** The policy as its JSON tree (a copy). */
  public JsonNode toJsonNode() {
    return tree.deepCopy();
  }

  /** The policy as JSON text. */
  public String toJson() {
    return tree.toString();
  }

  @Override
  public String toString() {
    return toJson();
  }

  @Override
  public boolean equals(Object o) {
    return o instanceof Policy p && p.toJson().equals(toJson());
  }

  @Override
  public int hashCode() {
    return toJson().hashCode();
  }

  private static Policy node(String key, JsonNode value) {
    ObjectNode o = F.objectNode();
    o.set(key, value);
    return new Policy(o);
  }

  private static Policy list(String key, Policy... children) {
    ArrayNode a = F.arrayNode();
    for (Policy p : children) {
      a.add(p.tree);
    }
    return node(key, a);
  }

  /** Matches when every child matches (unknown stays unknown). */
  public static Policy allOf(Policy... children) {
    return list("all_of", children);
  }

  /** Matches when any child matches. */
  public static Policy anyOf(Policy... children) {
    return list("any_of", children);
  }

  /** Matches when no child matches. */
  public static Policy noneOf(Policy... children) {
    return list("none_of", children);
  }

  /** Requires the scope among the evidence's scopes. */
  public static Policy scope(String scope) {
    return node("scope", TextNode.valueOf(scope));
  }

  /** Requires the tag among the evidence's tags. */
  public static Policy tag(String tag) {
    return node("tag", TextNode.valueOf(tag));
  }

  /** Requires the audience among the evidence's audiences. */
  public static Policy audience(String audience) {
    return node("audience", TextNode.valueOf(audience));
  }

  /** Requires a direct path from inside the network (e.g. {@code "10.0.0.0/8"}). */
  public static Policy sourceCidr(String cidr) {
    return node("source_cidr", TextNode.valueOf(cidr));
  }

  /** Requires a typed attribute value. */
  public static Policy attributeEquals(String key, Attribute value) {
    ObjectNode v = F.objectNode();
    v.put("type", value.type());
    v.set("value", MAPPER.valueToTree(value.value()));
    ObjectNode o = F.objectNode();
    o.put("key", key);
    o.set("value", v);
    return node("attribute_equals", o);
  }

  /**
   * Explicitly accepts cached / offline policy (known revocations still deny). The default policy.
   */
  public static Policy tolerateStale() {
    return new Policy(TextNode.valueOf("tolerate_stale_policy"));
  }

  /**
   * Requires a policy synchronization at most {@code maxAge} old (from the root itself when {@code
   * requireRoot}).
   */
  public static Policy synchronizedWithin(Duration maxAge, boolean requireRoot) {
    ObjectNode o = F.objectNode();
    o.put("max_age_secs", maxAge.toSeconds());
    o.put("require_root", requireRoot);
    return node("policy_synchronized_within", o);
  }

  /** Admits only peers the root registered (a presented chain alone does not match). */
  public static Policy requireRegistration() {
    return new Policy(TextNode.valueOf("require_registration"));
  }

  /** Requires the key (64 hex) on the presented chain's issuer path. */
  public static Policy issuedBy(String key) {
    return node("issued_by", TextNode.valueOf(key));
  }

  /** Requires evidence issued at most {@code maxAge} ago. */
  public static Policy maxCredentialAge(Duration maxAge) {
    return node("max_credential_age_secs", F.numberNode(maxAge.toSeconds()));
  }
}
