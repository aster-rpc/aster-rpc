package site.aster.identity;

import com.fasterxml.jackson.databind.JsonNode;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Replaces a node's whole {@code aster.ops.NodeConfig} record (Rust {@code NodeConfigSetRequest}).
 *
 * @param expectedGeneration 0 = unconditional; otherwise the set is {@code ABORTED} unless it
 *     matches
 * @param attributes typed attributes; keys may not start with {@code aster.}
 * @param identity overrides {@code [identity]} ({@code null}: not overridden)
 * @param sections other section overrides by name, as JSON objects
 */
public record NodeConfigSetRequest(
    long expectedGeneration,
    String name,
    List<String> tags,
    Map<String, NodeAttribute> attributes,
    IdentityOverride identity,
    Map<String, JsonNode> sections) {

  public NodeConfigSetRequest {
    name = name == null ? "" : name;
    tags = tags == null ? List.of() : List.copyOf(tags);
    attributes = attributes == null ? Map.of() : Map.copyOf(attributes);
    sections = sections == null ? Map.of() : Map.copyOf(sections);
  }

  /** An empty record (name, tags and attributes cleared, nothing overridden). */
  public static NodeConfigSetRequest empty() {
    return new NodeConfigSetRequest(0, "", List.of(), Map.of(), null, Map.of());
  }

  public NodeConfigSetRequest withExpectedGeneration(long v) {
    return new NodeConfigSetRequest(v, name, tags, attributes, identity, sections);
  }

  public NodeConfigSetRequest withName(String v) {
    return new NodeConfigSetRequest(expectedGeneration, v, tags, attributes, identity, sections);
  }

  public NodeConfigSetRequest withTags(List<String> v) {
    return new NodeConfigSetRequest(expectedGeneration, name, v, attributes, identity, sections);
  }

  public NodeConfigSetRequest withAttribute(String key, NodeAttribute value) {
    Map<String, NodeAttribute> m = new LinkedHashMap<>(attributes);
    m.put(key, value);
    return new NodeConfigSetRequest(expectedGeneration, name, tags, m, identity, sections);
  }

  public NodeConfigSetRequest withIdentity(IdentityOverride v) {
    return new NodeConfigSetRequest(expectedGeneration, name, tags, attributes, v, sections);
  }

  public NodeConfigSetRequest withSection(String section, JsonNode value) {
    Map<String, JsonNode> m = new LinkedHashMap<>(sections);
    m.put(section, value);
    return new NodeConfigSetRequest(expectedGeneration, name, tags, attributes, identity, m);
  }

  /** The {@code aster_node_config_call} SET body (without {@code deadline_ms}). */
  Map<String, Object> toJson() {
    Map<String, Object> s = new LinkedHashMap<>(sections);
    if (identity != null) {
      s.put("identity", identity);
    }
    Map<String, Object> out = new LinkedHashMap<>();
    out.put("expected_generation", expectedGeneration);
    out.put("name", name);
    out.put("tags", tags);
    out.put("attributes", attributes);
    out.put("sections", s);
    return out;
  }
}
