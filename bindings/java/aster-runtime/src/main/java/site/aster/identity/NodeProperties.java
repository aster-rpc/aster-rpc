package site.aster.identity;

import java.util.List;
import java.util.Map;

/**
 * A node's {@code aster.ops.NodeConfig} properties as handlers see them now (Rust {@code
 * aster::rpc::NodeProperties}; {@code aster_server_node_properties}). Attributes never authorise.
 *
 * @param generation the NodeConfig generation in effect (0: never changed, or NodeConfig off)
 */
public record NodeProperties(
    long generation, String name, List<String> tags, Map<String, NodeAttribute> attributes) {

  /** No properties (NodeConfig off, or no server). */
  public static final NodeProperties EMPTY = new NodeProperties(0, "", List.of(), Map.of());

  public NodeProperties {
    name = name == null ? "" : name;
    tags = tags == null ? List.of() : List.copyOf(tags);
    attributes = attributes == null ? Map.of() : Map.copyOf(attributes);
  }

  /** Whether the node carries {@code tag}. */
  public boolean hasTag(String tag) {
    return tags.contains(tag);
  }

  /** One attribute, or {@code null}. */
  public NodeAttribute attribute(String key) {
    return attributes.get(key);
  }
}
